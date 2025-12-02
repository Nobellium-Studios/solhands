use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_lang::solana_program::{program::invoke_signed, system_instruction};
use sha2::{Digest, Sha256};

declare_id!("G7Z1FnF9np177M8gCYhn3sudAZsoms1C8UiHhBmYNWSU");

// 1% rake = 100 basis points
const DEFAULT_HOUSE_FEE_BPS: u16 = 100;
const MAX_HOUSE_FEE_BPS: u16 = 1_000; // e.g. max 10%
const BPS_DENOMINATOR: u64 = 10_000;
const MAX_ROUNDS: usize = 5;
const MIN_BET_LAMPORTS: u64 = 100_000_000;
// Game timeout (e.g. if player2 never joins)
// ~3 minutes at 400ms/slot = 180s / 0.4s = 450 slots
const TIMEOUT_SLOTS: u64 = 450;
// Estimated block time on Solana mainnet/devnet ~400ms. Used to map seconds to slots.
const ESTIMATED_SLOT_MS: u64 = 400;
const COMMIT_PHASE_MS: u64 = 30_000; // 30 seconds to allow for network latency and signing
//const REVEAL_PHASE_MS: u64 = 5_000;
// Convert the 5 second windows into slots (rounded up) so on-chain deadlines track block time.
const COMMIT_PHASE_SLOTS: u64 = (COMMIT_PHASE_MS + ESTIMATED_SLOT_MS - 1) / ESTIMATED_SLOT_MS;
//const REVEAL_PHASE_SLOTS: u64 = (REVEAL_PHASE_MS + ESTIMATED_SLOT_MS - 1) / ESTIMATED_SLOT_MS;

fn transfer_with_signer<'info>(
    amount: u64,
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    system_program: &Program<'info, System>,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }

    let ix = system_instruction::transfer(from.key, to.key, amount);

    invoke_signed(
        &ix,
        &[
            from.clone(),
            to.clone(),
            system_program.to_account_info(),
        ],
        signer_seeds,
    )?;

    Ok(())
}

#[program]
pub mod rps_game {
    use super::*;

    pub fn init_house_vault(ctx: Context<InitHouseVault>) -> Result<()> {
        let vault = &mut ctx.accounts.house_vault;
        vault.bump = ctx.bumps.house_vault;
        vault.admin = ctx.accounts.admin.key();
        vault.house_fee_bps = DEFAULT_HOUSE_FEE_BPS;
        Ok(())
    }

    pub fn set_house_fee(ctx: Context<SetHouseFee>, new_fee_bps: u16) -> Result<()> {
        let vault = &mut ctx.accounts.house_vault;

        require_keys_eq!(ctx.accounts.admin.key(), vault.admin, RpsError::Unauthorized);
        require!(new_fee_bps <= MAX_HOUSE_FEE_BPS, RpsError::InvalidHouseFee);

        vault.house_fee_bps = new_fee_bps;
        Ok(())
    }

    /// Player 1 creates the game and deposits entry fee + bet.
    ///
    /// - `game_id` is a 32-byte identifier (e.g. uuid bytes or hash of it)
    /// - `bet_amount` is per-player bet (lamports)
    /// - `entry_fee` is per-player fee (lamports, non-refundable)
    pub fn create_game(
        ctx: Context<CreateGame>,
        game_id: [u8; 32],
        bet_amount: u64,
        entry_fee: u64,
    ) -> Result<()> {
        // basic validation
        require!(bet_amount > 0, RpsError::InvalidBetAmount);
        require!(entry_fee > 0, RpsError::InvalidEntryFee);

        // enforce min bet = 0.1 SOL
        require!(
            bet_amount >= MIN_BET_LAMPORTS,
            RpsError::BetTooLow
        );

        // Player1 pays bet_amount into the per-game vault PDA
        let cpi_ctx_bet = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.player1.to_account_info(),
                to: ctx.accounts.game_vault.to_account_info(),
            },
        );
        system_program::transfer(cpi_ctx_bet, bet_amount)?;

        // Player1 pays entry_fee directly into global house vault SOL PDA
        let cpi_ctx_fee = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.player1.to_account_info(),
                to: ctx.accounts.house_vault_sol.to_account_info(),
            },
        );
        system_program::transfer(cpi_ctx_fee, entry_fee)?;

        // Init game state
        let game = &mut ctx.accounts.game;

        game.bump = ctx.bumps.game;
        game.game_id = game_id;

        game.player1 = ctx.accounts.player1.key();
        game.player2 = Pubkey::default();

        game.house_vault = ctx.accounts.house_vault.key();

        game.session_p1 = Pubkey::default();
        game.session_p2 = Pubkey::default();

        // store bet & entry fee in state
        game.bet_amount = bet_amount;
        game.entry_fee = entry_fee;

        // Only bets stay in the pot (lamports live in game_vault)
        game.total_pot = bet_amount;

        // snapshot current house fee
        game.house_fee_bps = ctx.accounts.house_vault.house_fee_bps;

        game.rounds_played = 0;
        game.player1_wins = 0;
        game.player2_wins = 0;
        game.status = GameStatus::WaitingForPlayer2;

        let clock = Clock::get()?;
        game.created_slot = clock.slot;

        game.commitments_p1 = [[0u8; 32]; MAX_ROUNDS];
        game.commitments_p2 = [[0u8; 32]; MAX_ROUNDS];
        game.committed_p1 = [false; MAX_ROUNDS];
        game.committed_p2 = [false; MAX_ROUNDS];
        game.moves_p1 = [0u8; MAX_ROUNDS];
        game.moves_p2 = [0u8; MAX_ROUNDS];
        game.revealed_p1 = [false; MAX_ROUNDS];
        game.revealed_p2 = [false; MAX_ROUNDS];
        game.commit_deadline_slots = [0u64; MAX_ROUNDS];
        //game.reveal_deadline_slots = [0u64; MAX_ROUNDS];
        game.round_resolved = [false; MAX_ROUNDS];

        Ok(())
    }


    /// Player 2 joins the game and deposits the same entry fee + bet.
    pub fn join_game(ctx: Context<JoinGame>) -> Result<()> {
        let game = &mut ctx.accounts.game;

        // Game must be open for Player2
        require!(
            game.status == GameStatus::WaitingForPlayer2,
            RpsError::GameNotJoinable
        );
        require!(
            game.player2 == Pubkey::default(),
            RpsError::AlreadyHasPlayer2
        );

        // Canonical amounts from on-chain state (Player 2
        // cannot choose their own bet/fee)
        let bet_amount = game.bet_amount;
        let entry_fee = game.entry_fee;

        // Same business rules as create_game
        require!(bet_amount > 0, RpsError::InvalidBetAmount);
        require!(entry_fee > 0, RpsError::InvalidEntryFee);
        require!(bet_amount >= MIN_BET_LAMPORTS, RpsError::BetTooLow);

        // Player2 pays bet into game_vault
        let cpi_ctx_bet = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.player2.to_account_info(),
                to: ctx.accounts.game_vault.to_account_info(),
            },
        );
        system_program::transfer(cpi_ctx_bet, bet_amount)?;

        // Player2 pays entry fee into house_vault_sol
        let cpi_ctx_fee = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.player2.to_account_info(),
                to: ctx.accounts.house_vault_sol.to_account_info(),
            },
        );
        system_program::transfer(cpi_ctx_fee, entry_fee)?;

        // Update game state (only bets remain in the pot)
        game.player2 = ctx.accounts.player2.key();
        game.total_pot = game
            .total_pot
            .checked_add(bet_amount)
            .ok_or(RpsError::MathOverflow)?;

        // keep your existing next status, unless you want a more specific one
        game.status = GameStatus::Active;

        Ok(())
    }

        /// Starts a round and opens the 5-second commit window on-chain.
    ///
    /// - Can be called by player1 / player2 or their session signers.
    /// - Sets commit_deadline_slots[round] based on current slot.
    /// - If already started or resolved, reverts.
    pub fn start_round(
        ctx: Context<StartRound>,
        round_index: u8,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;

        require!(game.status == GameStatus::Active, RpsError::GameNotActive);
        require!((round_index as usize) < MAX_ROUNDS, RpsError::InvalidRound);

        let idx = round_index as usize;

        // Aynı round'u ikinci defa başlatma
        require!(
            game.commit_deadline_slots[idx] == 0,
            RpsError::CommitWindowAlreadyStarted
        );
        require!(
            !game.round_resolved[idx],
            RpsError::RoundAlreadyResolved
        );

        let current_slot = Clock::get()?.slot;
        let deadline = current_slot
            .checked_add(COMMIT_PHASE_SLOTS)
            .ok_or(RpsError::MathOverflow)?;

        game.commit_deadline_slots[idx] = deadline;

        emit!(RoundStartEvent {
            game_id: game.game_id,
            round: round_index,
            start_slot: current_slot,
            commit_deadline_slot: deadline,
        });

        Ok(())
    }


    /// Commit move for a given round.
    ///
    /// - Only stores `commitment` (the 32-byte hash)
    /// - Player identity is inferred from the signer account
    pub fn commit_move(
        ctx: Context<CommitMove>,
        round_index: u8,
        commitment: [u8; 32],
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;
        let player = &ctx.accounts.player;

        require!(game.status == GameStatus::Active, RpsError::GameNotActive);
        require!((round_index as usize) < MAX_ROUNDS, RpsError::InvalidRound);

        let idx = round_index as usize;
        let current_slot = Clock::get()?.slot;

        // koray - 28.11.2025 - Commit window MUST have been started by start_round.
        let deadline = game.commit_deadline_slots[idx];
        require!(
            deadline != 0,
            RpsError::CommitWindowNotStarted
        );
        require!(
            current_slot <= deadline,
            RpsError::CommitPhaseExpired
        );

        // ---- IMPORTANT: correctly classify who this signer is ----
        let pk = player.key();
        let is_p1 = pk == game.player1 || pk == game.session_p1;
        let is_p2 = pk == game.player2 || pk == game.session_p2;
        require!(is_p1 || is_p2, RpsError::NotAPlayer);

        if is_p1 {
            require!(!game.committed_p1[idx], RpsError::AlreadyCommitted);
            game.commitments_p1[idx] = commitment;
            game.committed_p1[idx] = true;
        } else {
            require!(!game.committed_p2[idx], RpsError::AlreadyCommitted);
            game.commitments_p2[idx] = commitment;
            game.committed_p2[idx] = true;
        }

        // When both commits are in, start the reveal window and notify clients.
        let both_committed = game.committed_p1[idx] && game.committed_p2[idx];

        emit!(RoundPhaseEvent {
            game_id: game.game_id,
            round: round_index,
            current_slot,
            commit_deadline_slot: game.commit_deadline_slots[idx],
            reveal_deadline_slot: 0, // CHANGED: no reveal deadline on-chain
            both_committed,
        });

        Ok(())
    }


    /// Reveal move for a given round.
    ///
    /// - Verifies hash(move || nonce || game_id || round_index || player_pubkey)
    ///   matches previously stored commitment.
    /// - If both players revealed, computes round winner and possibly finishes game.
    pub fn reveal_move(
        ctx: Context<RevealMove>,
        round_index: u8,
        move_value: u8,
        nonce: [u8; 32],
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;
        let player = &ctx.accounts.player;

        require!(game.status == GameStatus::Active, RpsError::GameNotActive);
        require!((round_index as usize) < MAX_ROUNDS, RpsError::InvalidRound);
        require!(move_value <= 2, RpsError::InvalidMove);

        let idx = round_index as usize;

    // koray - 28.11.2025 CHANGED: no reveal time limit, but enforce both commits and not resolved
    require!(
        game.committed_p1[idx] && game.committed_p2[idx],
        RpsError::BothMustCommitFirst
    );
    require!(
        !game.round_resolved[idx],
        RpsError::RoundAlreadyResolved
    );

    let pk = player.key();
    let is_p1 = pk == game.player1 || pk == game.session_p1;
    let is_p2 = pk == game.player2 || pk == game.session_p2;
    require!(is_p1 || is_p2, RpsError::NotAPlayer);

        // recompute commitment hash
        // Use main wallet pubkey (not session key) for commitment verification
        // because Unity computes commitment with main wallet
        let commitment_pubkey = if is_p1 { game.player1 } else { game.player2 };

        let mut hasher = Sha256::new();
        hasher.update(&[move_value]);
        hasher.update(&nonce);
        hasher.update(&game.game_id);
        hasher.update(&[round_index]);
        hasher.update(commitment_pubkey.as_ref());
        let hash = hasher.finalize();
        let mut hash_bytes = [0u8; 32];
        hash_bytes.copy_from_slice(&hash[..]);

        if is_p1 {
            require!(game.committed_p1[idx], RpsError::NotCommittedYet);
            require!(!game.revealed_p1[idx], RpsError::AlreadyRevealed);
            require!(
                game.commitments_p1[idx] == hash_bytes,
                RpsError::CommitmentMismatch
            );
            game.moves_p1[idx] = move_value;
            game.revealed_p1[idx] = true;
        } else {
            require!(game.committed_p2[idx], RpsError::NotCommittedYet);
            require!(!game.revealed_p2[idx], RpsError::AlreadyRevealed);
            require!(
                game.commitments_p2[idx] == hash_bytes,
                RpsError::CommitmentMismatch
            );
            game.moves_p2[idx] = move_value;
            game.revealed_p2[idx] = true;
        }

        // resolution logic unchanged...
        if game.revealed_p1[idx] && game.revealed_p2[idx] {
            let m1 = game.moves_p1[idx];
            let m2 = game.moves_p2[idx];
            let round_result = round_winner(m1, m2);

            match round_result {
                RoundResult::Player1Win => {
                    game.player1_wins = game
                        .player1_wins
                        .checked_add(1)
                        .ok_or(RpsError::MathOverflow)?;
                }
                RoundResult::Player2Win => {
                    game.player2_wins = game
                        .player2_wins
                        .checked_add(1)
                        .ok_or(RpsError::MathOverflow)?;
                }
                RoundResult::Draw => { /* no change */ }
            }

            game.rounds_played = game
                .rounds_played
                .checked_add(1)
                .ok_or(RpsError::MathOverflow)?;

            game.round_resolved[idx] = true;

            if game.player1_wins >= 3
                || game.player2_wins >= 3
                || game.rounds_played >= MAX_ROUNDS as u8
            {
                game.status = GameStatus::Finished;
            }

            emit!(RoundResultEvent {
                game_id: game.game_id,
                round: round_index,
                player1_wins: game.player1_wins,
                player2_wins: game.player2_wins,
                rounds_played: game.rounds_played,
                status: game.status,
            });
        }

        Ok(())
    }

        /// Resolves a round by timeout after the commit window has expired.
    ///
    /// - Can be called by anyone (mediator, any user).
    /// - Only allowed if:
    ///   * Game is Active
    ///   * Commit window was started for that round
    ///   * Current slot > commit_deadline_slots[round]
    ///   * Round not already resolved
    ///   * NOT both players committed (if both committed, must use reveal_move)
    /// - Outcome rules:
    ///   * Only P1 committed  -> P1 wins the round
    ///   * Only P2 committed  -> P2 wins the round
    ///   * None committed     -> Draw
    pub fn resolve_commit_timeout(
        ctx: Context<ResolveCommitTimeout>,
        round_index: u8,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;

        require!(game.status == GameStatus::Active, RpsError::GameNotActive);
        require!((round_index as usize) < MAX_ROUNDS, RpsError::InvalidRound);

        let idx = round_index as usize;
        let current_slot = Clock::get()?.slot;

        // Commit penceresi başlatılmış olmalı
        require!(
            game.commit_deadline_slots[idx] != 0,
            RpsError::CommitWindowNotStarted
        );
        // Ve commit süresi bitmiş olmalı
        require!(
            current_slot > game.commit_deadline_slots[idx],
            RpsError::CommitPhaseNotExpired
        );
        // Aynı round'u ikinci kere resolve etmeyelim
        require!(
            !game.round_resolved[idx],
            RpsError::RoundAlreadyResolved
        );

        let c1 = game.committed_p1[idx];
        let c2 = game.committed_p2[idx];

        // Eğer iki taraf da commit ettiyse time-out resolve yasak,
        // mutlaka reveal ile devam etmelisin.
        require!(!(c1 && c2), RpsError::BothCommittedNoTimeout);

        let result = if c1 && !c2 {
            RoundResult::Player1Win
        } else if !c1 && c2 {
            RoundResult::Player2Win
        } else {
            RoundResult::Draw
        };

        match result {
            RoundResult::Player1Win => {
                game.player1_wins = game
                    .player1_wins
                    .checked_add(1)
                    .ok_or(RpsError::MathOverflow)?;
            }
            RoundResult::Player2Win => {
                game.player2_wins = game
                    .player2_wins
                    .checked_add(1)
                    .ok_or(RpsError::MathOverflow)?;
            }
            RoundResult::Draw => { /* no change */ }
        }

        game.rounds_played = game
            .rounds_played
            .checked_add(1)
            .ok_or(RpsError::MathOverflow)?;

        game.round_resolved[idx] = true;

        if game.player1_wins >= 3
            || game.player2_wins >= 3
            || game.rounds_played >= MAX_ROUNDS as u8
        {
            game.status = GameStatus::Finished;
        }

        emit!(RoundResultEvent {
            game_id: game.game_id,
            round: round_index,
            player1_wins: game.player1_wins,
            player2_wins: game.player2_wins,
            rounds_played: game.rounds_played,
            status: game.status,
        });

        Ok(())
    }

    /// Forfeit game - ends the game immediately and declares a winner.
    ///
    /// - Called when a player disconnects, times out, or abandons the game.
    /// - Can be called by anyone (mediator, player, or any user).
    /// - The caller must specify who forfeited (loser).
    /// - The other player wins by default (3 wins credited).
    /// - Game status is set to Finished, ready for settlement.
    pub fn forfeit_game(
        ctx: Context<ForfeitGame>,
        loser_is_player1: bool,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;

        // Game must be Active
        require!(game.status == GameStatus::Active, RpsError::GameNotActive);

        // Set the winner
        if loser_is_player1 {
            // PLAYER1 forfeited/disconnected -> PLAYER2 wins
            game.player2_wins = 3;
            msg!("PLAYER1 forfeited. PLAYER2 wins!");
        } else {
            // PLAYER2 forfeited/disconnected -> PLAYER1 wins
            game.player1_wins = 3;
            msg!("PLAYER2 forfeited. PLAYER1 wins!");
        }

        // Mark game as finished
        game.status = GameStatus::Finished;

        emit!(GameForfeitEvent {
            game_id: game.game_id,
            loser: if loser_is_player1 { game.player1 } else { game.player2 },
            winner: if loser_is_player1 { game.player2 } else { game.player1 },
        });

        Ok(())
    }

    /// Cancel game - refunds both players their bets.
    ///
    /// - Called when there's an error (blockchain timeout, commit phase expired, etc.)
    /// - Can be called by anyone (mediator, player, or any user).
    /// - Both players get their bets refunded (no house fee).
    /// - Game status is set to Cancelled.
    pub fn cancel_game(ctx: Context<CancelGame>) -> Result<()> {
        let game = &mut ctx.accounts.game;

        // Game must be Active (not already Finished/Settled/Cancelled)
        require!(game.status == GameStatus::Active, RpsError::GameNotActive);

        // Mark game as Cancelled
        game.status = GameStatus::Cancelled;

        let total_pot = game.total_pot;
        let bet_amount = game.bet_amount;

        // Calculate refunds - each player gets their bet back
        // If only player1 has deposited, they get full refund
        // If both players deposited, each gets their bet back
        let player1_refund = bet_amount;
        let player2_refund = if total_pot >= bet_amount * 2 {
            bet_amount
        } else if total_pot > bet_amount {
            total_pot - bet_amount
        } else {
            0
        };

        msg!("Cancelling game. Refunding player1: {} lamports, player2: {} lamports",
             player1_refund, player2_refund);

        // Transfer refunds from game vault
        let game_vault = &ctx.accounts.game_vault;
        let player1 = &ctx.accounts.player1;
        let player2 = &ctx.accounts.player2;

        // Use game_id as seeds for the vault PDA (must match "game_vault" seed used in create_game)
        let game_id = game.game_id;
        let vault_seeds: &[&[u8]] = &[
            b"game_vault",
            &game_id,
            &[ctx.bumps.game_vault],
        ];
        let vault_signer = &[vault_seeds];

        // Refund player1
        if player1_refund > 0 {
            let transfer_ix_p1 = anchor_lang::solana_program::system_instruction::transfer(
                &game_vault.key(),
                &player1.key(),
                player1_refund,
            );
            anchor_lang::solana_program::program::invoke_signed(
                &transfer_ix_p1,
                &[
                    game_vault.to_account_info(),
                    player1.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                ],
                vault_signer,
            )?;
        }

        // Refund player2
        if player2_refund > 0 {
            let transfer_ix_p2 = anchor_lang::solana_program::system_instruction::transfer(
                &game_vault.key(),
                &player2.key(),
                player2_refund,
            );
            anchor_lang::solana_program::program::invoke_signed(
                &transfer_ix_p2,
                &[
                    game_vault.to_account_info(),
                    player2.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                ],
                vault_signer,
            )?;
        }

        emit!(GameCancelledEvent {
            game_id: game.game_id,
            player1: game.player1,
            player2: game.player2,
            player1_refund,
            player2_refund,
        });

        Ok(())
    }


    /// Settles the game: pays the winner, house fee, or splits pot.
    ///
    /// - Can be called by anyone once `status == Finished`.
    /// - Transfers from `game_vault` account to `winner` & `house_vault_sol`.
    /// - Closes `game` account and returns remaining rent to `player1`.
    pub fn settle_game(ctx: Context<SettleGame>) -> Result<()> {
        let game = &mut ctx.accounts.game;

        require!(
            game.status == GameStatus::Finished,
            RpsError::GameNotFinished
        );
        game.status = GameStatus::Settled;

        let total_pot = game.total_pot;
        require!(total_pot > 0, RpsError::InvalidBetAmount);

        let player1 = &ctx.accounts.player1;
        let player2 = &ctx.accounts.player2;

        // Winner determination
        let winner: Option<Pubkey> = if game.player1_wins > game.player2_wins {
            Some(game.player1)
        } else if game.player2_wins > game.player1_wins {
            Some(game.player2)
        } else {
            None
        };

        let (payout_p1, payout_p2, house_fee) = if let Some(winner_pk) = winner {
            let house_fee = total_pot
                .checked_mul(game.house_fee_bps as u64)
                .ok_or(RpsError::MathOverflow)?
                .checked_div(BPS_DENOMINATOR)
                .ok_or(RpsError::MathOverflow)?;

            let winner_amount = total_pot
                .checked_sub(house_fee)
                .ok_or(RpsError::MathOverflow)?;

            if winner_pk == game.player1 {
                (winner_amount, 0, house_fee)
            } else {
                (0, winner_amount, house_fee)
            }
        } else {
            // draw: split pot, no rake
            let half = total_pot
                .checked_div(2)
                .ok_or(RpsError::MathOverflow)?;
            let remainder = total_pot
                .checked_sub(half.checked_mul(2).ok_or(RpsError::MathOverflow)?)
                .ok_or(RpsError::MathOverflow)?;
            (half + remainder, half, 0)
        };

        // seeds for the system-owned game_vault PDA
        let game_vault_bump = ctx.bumps.game_vault;
        let seeds: &[&[u8]] = &[
            b"game_vault",
            game.game_id.as_ref(),
            &[game_vault_bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[seeds];

        let game_vault_ai = ctx.accounts.game_vault.to_account_info();
        let system_program = &ctx.accounts.system_program;

        // payouts from game_vault
        transfer_with_signer(
            payout_p1,
            &game_vault_ai,
            &player1,
            system_program,
            signer_seeds,
        )?;
        transfer_with_signer(
            payout_p2,
            &game_vault_ai,
            &player2,
            system_program,
            signer_seeds,
        )?;
        transfer_with_signer(
            house_fee,
            &game_vault_ai,
            &ctx.accounts.house_vault_sol.to_account_info(),
            system_program,
            signer_seeds,
        )?;

        // Anchor will close `game` and send its rent to player1 due to `close = player1`
        Ok(())
    }

    /// Withdraws SOL from the global house vault PDA to the admin wallet.
    ///
    /// - Only the stored `admin` in `HouseVault` is allowed to call this.
    /// - Signs with the `house_vault_sol` PDA seeds.
    pub fn withdraw_house_funds(
        ctx: Context<WithdrawHouseFunds>,
        amount: u64,
    ) -> Result<()> {
        // Admin auth is enforced by account constraint (address = house_vault.admin)

        let bump = ctx.bumps.house_vault_sol;
        let seeds: &[&[u8]] = &[
            b"house_vault_sol",
            &[bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[seeds];

        transfer_with_signer(
            amount,
            &ctx.accounts.house_vault_sol.to_account_info(),
            &ctx.accounts.admin.to_account_info(),
            &ctx.accounts.system_program,
            signer_seeds,
        )
    }

    /// Authorize a delegated session signer for this game.
    ///
    /// - The `player` must be either player1 or player2.
    /// - `session_pubkey` will be allowed to call commit_move / reveal_move for this game.
    pub fn authorize_session_signer(
        ctx: Context<AuthorizeSessionSigner>,
        session_pubkey: Pubkey,
    ) -> Result<()> {
        let game = &mut ctx.accounts.game;
        let player_key = ctx.accounts.player.key();

        // Optional: only allow while game is not finished
        require!(
            game.status == GameStatus::WaitingForPlayer2 || game.status == GameStatus::Active,
            RpsError::InvalidGameState
        );

        if player_key == game.player1 {
            game.session_p1 = session_pubkey;
        } else if player_key == game.player2 {
            game.session_p2 = session_pubkey;
        } else {
            return Err(RpsError::NotAPlayer.into());
        }

        Ok(())
    }

    /// Allows anyone (mediator or player1) to cancel a game that never started
    /// (player2 never joined) after TIMEOUT_SLOTS have passed since creation.
    ///
    /// - Refunds player1's bet from game_vault (entry fee stays with house).
    pub fn cancel_game_if_timed_out(ctx: Context<CancelGameIfTimedOut>) -> Result<()> {
        let game = &ctx.accounts.game;

        require!(
            game.status == GameStatus::WaitingForPlayer2,
            RpsError::GameNotCancellable
        );
        // player1 validation is done via constraint in the accounts struct

        let current_slot = Clock::get()?.slot;
        require!(
            current_slot >= game.created_slot + TIMEOUT_SLOTS,
            RpsError::NotTimedOut
        );

        // refund pot to player1 from game_vault
        let amount = game.total_pot;
        if amount > 0 {
            let bump = ctx.bumps.game_vault;
            let seeds: &[&[u8]] = &[
                b"game_vault",
                game.game_id.as_ref(),
                &[bump],
            ];
            let signer_seeds: &[&[&[u8]]] = &[seeds];

            transfer_with_signer(
                amount,
                &ctx.accounts.game_vault.to_account_info(),
                &ctx.accounts.player1.to_account_info(),
                &ctx.accounts.system_program,
                signer_seeds,
            )?;
        }

        msg!("Game cancelled due to timeout. Refunded {} lamports to player1", amount);

        // Anchor will close game and send its rent to player1
        Ok(())
    }
}

// ---------- Helpers ----------

/// 0 = Rock, 1 = Paper, 2 = Scissors
fn round_winner(m1: u8, m2: u8) -> RoundResult {
    use RoundResult::*;
    if m1 == m2 {
        return Draw;
    }
    match (m1, m2) {
        (0, 2) | (1, 0) | (2, 1) => Player1Win,
        (2, 0) | (0, 1) | (1, 2) => Player2Win,
        _ => Draw,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RoundResult {
    Player1Win,
    Player2Win,
    Draw,
}

// ---------- Accounts & State ----------

#[derive(Accounts)]
pub struct WithdrawHouseFunds<'info> {
    #[account(
        mut,
        address = house_vault.admin @ RpsError::Unauthorized
    )]
    pub admin: Signer<'info>,

    #[account(
        seeds = [b"house_vault"],
        bump = house_vault.bump,
    )]
    pub house_vault: Account<'info, HouseVault>,

    /// CHECK: This is a PDA vault for house funds. Its address is verified by seeds and bump,
    /// and we only use it as a lamport holder (no deserialization).
    #[account(
        mut,
        seeds = [b"house_vault_sol"],
        bump,
        owner = system_program::ID
    )]
    pub house_vault_sol: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct StartRound<'info> {
    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,

    /// Must be a player or their authorized session key
    #[account(
        constraint =
            caller.key() == game.player1 ||
            caller.key() == game.player2 ||
            caller.key() == game.session_p1 ||
            caller.key() == game.session_p2
            @ RpsError::NotAPlayer
    )]
    pub caller: Signer<'info>,
}


#[derive(Accounts)]
pub struct ResolveCommitTimeout<'info> {
    /// Anyone can call this (mediator, player, random user).
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,
}

#[derive(Accounts)]
pub struct ForfeitGame<'info> {
    /// Must be one of the players or their session key
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,
}

#[derive(Accounts)]
pub struct CancelGame<'info> {
    /// Anyone can call cancel_game
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,

    /// CHECK: Player 1 account to receive refund
    #[account(
        mut,
        constraint = player1.key() == game.player1 @ RpsError::InvalidPlayerAccount
    )]
    pub player1: AccountInfo<'info>,

    /// CHECK: Player 2 account to receive refund
    #[account(
        mut,
        constraint = player2.key() == game.player2 @ RpsError::InvalidPlayerAccount
    )]
    pub player2: AccountInfo<'info>,

    /// Game vault PDA that holds the bet
    /// CHECK: This is a PDA that holds SOL, not an Anchor account
    #[account(
        mut,
        seeds = [b"game_vault", &game.game_id],
        bump
    )]
    pub game_vault: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AuthorizeSessionSigner<'info> {
    #[account(mut)]
    pub player: Signer<'info>,

    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump,
        constraint = player.key() == game.player1 || player.key() == game.player2
            @ RpsError::NotAPlayer
    )]
    pub game: Account<'info, Game>,
}

#[derive(Accounts)]
pub struct CancelGameIfTimedOut<'info> {
    /// Anyone can call (mediator or player1) - no signer restriction
    #[account(mut)]
    pub caller: Signer<'info>,

    /// CHECK: Player 1 account to receive refund - validated against game.player1
    #[account(
        mut,
        constraint = player1.key() == game.player1 @ RpsError::NotAPlayer
    )]
    pub player1: UncheckedAccount<'info>,

    #[account(
        mut,
        close = player1,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,

    /// CHECK: This is the PDA vault holding the game pot. Address is enforced via seeds and bump,
    /// and we only move lamports from it (no data layout is assumed).
    #[account(
        mut,
        seeds = [b"game_vault", &game.game_id],
        bump,
        owner = system_program::ID
    )]
    pub game_vault: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetHouseFee<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [b"house_vault"],
        bump = house_vault.bump,
    )]
    pub house_vault: Account<'info, HouseVault>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GameStatus {
    WaitingForPlayer2 = 0,
    Active = 1,
    Finished = 2,
    Cancelled = 3,
    Settled = 4, // payouts done, cannot be settled again
}

#[account]
pub struct HouseVault {
    pub bump: u8,
    pub admin: Pubkey,      // who is allowed to withdraw / change fee
    pub house_fee_bps: u16, // current global fee configuration
}

impl HouseVault {
    pub const SPACE: usize = 8 // discriminator
        + 1                    // bump
        + 32                   // admin
        + 2;                   // house_fee_bps
}

#[account]
pub struct Game {
    pub bump: u8,
    pub game_id: [u8; 32],

    pub player1: Pubkey,
    pub player2: Pubkey,
    pub house_vault: Pubkey,

    pub session_p1: Pubkey, // delegated signer that can act as player1
    pub session_p2: Pubkey, // delegated signer that can act as player2

    pub bet_amount: u64,
    pub entry_fee: u64,
    pub total_pot: u64,
    pub house_fee_bps: u16,

    pub rounds_played: u8,
    pub player1_wins: u8,
    pub player2_wins: u8,
    pub status: GameStatus,

    pub created_slot: u64, // for timeout logic

    // per-round commit / reveal data
    pub commitments_p1: [[u8; 32]; MAX_ROUNDS],
    pub commitments_p2: [[u8; 32]; MAX_ROUNDS],
    pub committed_p1: [bool; MAX_ROUNDS],
    pub committed_p2: [bool; MAX_ROUNDS],
    pub moves_p1: [u8; MAX_ROUNDS],
    pub moves_p2: [u8; MAX_ROUNDS],
    pub revealed_p1: [bool; MAX_ROUNDS],
    pub revealed_p2: [bool; MAX_ROUNDS],
    pub commit_deadline_slots: [u64; MAX_ROUNDS],
    //pub reveal_deadline_slots: [u64; MAX_ROUNDS],
    // koray-27.11.2025: to prevent double-resolution / reveals after timeout
    pub round_resolved: [bool; MAX_ROUNDS],
}

impl Game {
    pub const SPACE: usize = 8  // discriminator
        + 1                     // bump
        + 32                    // game_id
        + 32 * 3                // player1, player2, house_vault
        + 32 * 2                // session_p1, session_p2
        + 8 * 3                 // bet_amount, entry_fee, total_pot
        + 2                     // house_fee_bps
        + 1 * 4                 // rounds_played, p1_wins, p2_wins, status (u8)
        + 8                     // created_at
        + (32 * MAX_ROUNDS) * 2 // commitments_p1, commitments_p2
        + (1 * MAX_ROUNDS) * 2  // committed_p1, committed_p2
        + (1 * MAX_ROUNDS) * 2  // moves_p1, moves_p2
        + (1 * MAX_ROUNDS) * 2  // revealed_p1, revealed_p2
        + (8 * MAX_ROUNDS)      // commit_deadline_slots
        + (1 * MAX_ROUNDS);     // round_resolved
}


// ---------- Events ----------

#[event]
pub struct RoundPhaseEvent {
    pub game_id: [u8; 32],
    pub round: u8,
    pub current_slot: u64,
    pub commit_deadline_slot: u64,
    pub reveal_deadline_slot: u64,
    pub both_committed: bool,
}

#[event]
pub struct RoundResultEvent {
    pub game_id: [u8; 32],
    pub round: u8,
    pub player1_wins: u8,
    pub player2_wins: u8,
    pub rounds_played: u8,
    pub status: GameStatus,
}

#[event]
pub struct RoundStartEvent {
    pub game_id: [u8; 32],
    pub round: u8,
    pub start_slot: u64,
    pub commit_deadline_slot: u64,
}

#[event]
pub struct GameForfeitEvent {
    pub game_id: [u8; 32],
    pub loser: Pubkey,
    pub winner: Pubkey,
}

#[event]
pub struct GameCancelledEvent {
    pub game_id: [u8; 32],
    pub player1: Pubkey,
    pub player2: Pubkey,
    pub player1_refund: u64,
    pub player2_refund: u64,
}

// ---------- Instruction Contexts ----------

#[derive(Accounts)]
pub struct InitHouseVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = HouseVault::SPACE,
        seeds = [b"house_vault"],
        bump
    )]
    pub house_vault: Account<'info, HouseVault>,

    /// CHECK: PDA used as the on-chain SOL vault for house fees. Created and constrained by
    /// seeds + bump, only used as a lamport vault, never deserialized.
    #[account(
        init,
        payer = admin,
        space = 0,
        seeds = [b"house_vault_sol"],
        bump,
        owner = system_program::ID
    )]
    pub house_vault_sol: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(game_id: [u8; 32])]
pub struct CreateGame<'info> {
    #[account(mut)]
    pub player1: Signer<'info>,

    #[account(
        mut,
        seeds = [b"house_vault"],
        bump = house_vault.bump
    )]
    pub house_vault: Account<'info, HouseVault>,

    /// CHECK: House SOL vault PDA. We verify its address with seeds + bump and only use it
    /// as the recipient of entry fees (lamport transfers only).
    #[account(
        mut,
        seeds = [b"house_vault_sol"],
        bump,
        owner = system_program::ID
    )]
    pub house_vault_sol: UncheckedAccount<'info>,

    #[account(
        init,
        payer = player1,
        space = Game::SPACE,
        seeds = [b"game", game_id.as_ref()],
        bump
    )]
    pub game: Account<'info, Game>,

    /// CHECK: Per-game pot vault PDA. Address is derived via seeds + bump and only holds lamports.
    #[account(
        init,
        payer = player1,
        space = 0,
        seeds = [b"game_vault", game_id.as_ref()],
        bump,
        owner = system_program::ID
    )]
    pub game_vault: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct JoinGame<'info> {
    #[account(mut)]
    pub player2: Signer<'info>,

    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump,
        constraint = game.player1 != Pubkey::default() @ RpsError::InvalidGameState
    )]
    pub game: Account<'info, Game>,

    /// CHECK: Same per-game pot PDA created in `CreateGame`. Address checked via seeds + bump.
    #[account(
        mut,
        seeds = [b"game_vault", &game.game_id],
        bump,
        owner = system_program::ID
    )]
    pub game_vault: UncheckedAccount<'info>,

    #[account(
        seeds = [b"house_vault"],
        bump = house_vault.bump,
        constraint = house_vault.key() == game.house_vault @ RpsError::InvalidHouseWallet
    )]
    pub house_vault: Account<'info, HouseVault>,

    /// CHECK: Global house SOL vault PDA, same as in `InitHouseVault`/`CreateGame`. Address enforced
    /// via seeds + bump, used only for lamport transfers.
    #[account(
        mut,
        seeds = [b"house_vault_sol"],
        bump,
        owner = system_program::ID
    )]
    pub house_vault_sol: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CommitMove<'info> {
    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,

    #[account(
        mut,
        constraint =
            player.key() == game.player1 ||
            player.key() == game.player2 ||
            player.key() == game.session_p1 ||
            player.key() == game.session_p2
            @ RpsError::NotAPlayer
    )]
    pub player: Signer<'info>,
}

#[derive(Accounts)]
pub struct RevealMove<'info> {
    #[account(
        mut,
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,

    #[account(
        mut,
        constraint =
            player.key() == game.player1 ||
            player.key() == game.player2 ||
            player.key() == game.session_p1 ||
            player.key() == game.session_p2
            @ RpsError::NotAPlayer
    )]
    pub player: Signer<'info>,
}


#[derive(Accounts)]
pub struct SettleGame<'info> {
    #[account(
        mut,
        close = player1, // <-- let Anchor close & refund rent to player1
        seeds = [b"game", &game.game_id],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,

    /// CHECK: safe because of the `address = game.player1` constraint
    #[account(mut, address = game.player1 @ RpsError::InvalidPlayerAccount)]
    pub player1: AccountInfo<'info>,

    /// CHECK: safe because of the `address = game.player2` constraint
    #[account(mut, address = game.player2 @ RpsError::InvalidPlayerAccount)]
    pub player2: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"house_vault"],
        bump = house_vault.bump,
        constraint = house_vault.key() == game.house_vault @ RpsError::InvalidHouseWallet
    )]
    pub house_vault: Account<'info, HouseVault>,

    /// CHECK: House fee SOL vault PDA; address enforced via seeds + bump, only used for lamports.
    #[account(
        mut,
        seeds = [b"house_vault_sol"],
        bump,
        owner = system_program::ID
    )]
    pub house_vault_sol: UncheckedAccount<'info>,

    /// CHECK: Game pot SOL vault PDA; address enforced via seeds + bump, only used for lamports.
    #[account(
        mut,
        seeds = [b"game_vault", &game.game_id],
        bump,
        owner = system_program::ID
    )]
    pub game_vault: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

// ---------- Errors ----------

#[error_code]
pub enum RpsError {
    #[msg("Invalid bet amount")]
    InvalidBetAmount,
    #[msg("Invalid entry fee")]
    InvalidEntryFee,
    #[msg("Game is not joinable")]
    GameNotJoinable,
    #[msg("Game already has a second player")]
    AlreadyHasPlayer2,
    #[msg("Game is not active")]
    GameNotActive,
    #[msg("Game is not finished")]
    GameNotFinished,
    #[msg("Invalid round index")]
    InvalidRound,
    #[msg("Invalid move")]
    InvalidMove,
    #[msg("Player is not part of this game")]
    NotAPlayer,
    #[msg("Already committed for this round")]
    AlreadyCommitted,
    #[msg("Not committed yet for this round")]
    NotCommittedYet,
    #[msg("Already revealed for this round")]
    AlreadyRevealed,
    #[msg("Commitment hash does not match")]
    CommitmentMismatch,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid game state")]
    InvalidGameState,
    #[msg("Invalid house wallet")]
    InvalidHouseWallet,
    #[msg("Invalid player account")]
    InvalidPlayerAccount,
    #[msg("Unauthorized admin")]
    Unauthorized,
    #[msg("Game not cancellable in this state")]
    GameNotCancellable,
    #[msg("Game has not timed out yet")]
    NotTimedOut,
    #[msg("Invalid house fee bps")]
    InvalidHouseFee,
    #[msg("Bet is below minimum allowed")]
    BetTooLow,
    #[msg("Commit phase for this round has expired")]
    CommitPhaseExpired,
    #[msg("Both players must commit before reveal")]
    BothMustCommitFirst,
    #[msg("Round already resolved")]
    RoundAlreadyResolved,
    #[msg("Commit window not started for this round")]
    CommitWindowNotStarted,
    #[msg("Commit phase not yet expired")]
    CommitPhaseNotExpired,
    #[msg("Both players committed, timeout resolve not allowed")]
    BothCommittedNoTimeout,
    #[msg("Commit window already started for this round")]
    CommitWindowAlreadyStarted,
}
