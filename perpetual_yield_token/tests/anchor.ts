//Test file is still in review
//Feel free to make changes  
/* import * as anchor from "@coral-xyz/anchor";
import { BN } from "bn.js";
import assert from "assert";
import * as web3 from "@solana/web3.js";
import { TOKEN_PROGRAM_ID, Token } from "@solana/spl-token";
import type { PerpetualYieldToken } from "../target/types/perpetual_yield_token";

// Helper: Create a new SPL Token mint
async function createMint(provider: anchor.AnchorProvider): Promise<web3.PublicKey> {
  const mintKp = web3.Keypair.generate();
  const lamports = await provider.connection.getMinimumBalanceForRentExemption(82);
  const tx = new web3.Transaction().add(
    web3.SystemProgram.createAccount({
      fromPubkey: provider.publicKey,
      newAccountPubkey: mintKp.publicKey,
      lamports,
      space: 82,
      programId: TOKEN_PROGRAM_ID,
    })
  );
  tx.add(
    Token.createInitMintInstruction(
      TOKEN_PROGRAM_ID,
      mintKp.publicKey,
      9, // decimals
      provider.publicKey,
      null
    )
  );
  await provider.sendAndConfirm(tx, [mintKp]);
  return mintKp.publicKey;
}

describe("perpetual-yield-token", () => {
  // Configure the client to use the local cluster.
  anchor.setProvider(anchor.AnchorProvider.env());
  const provider = anchor.getProvider() as anchor.AnchorProvider;
  const program = anchor.workspace.PerpetualYieldToken as anchor.Program<PerpetualYieldToken>;

  it("initialize", async () => {
    // Generate keypair for the global state account.
    const globalStateKp = web3.Keypair.generate();
    // Create a new mint for testing.
    const tokenMint = await createMint(provider);

    // Define test parameters.
    const governance = provider.publicKey; // using provider's public key for testing
    const cooldownPeriod = new BN(604800); // 7 days in seconds
    const earlyWithdrawalPenalty = new BN(500); // 500 basis points (5%)
    const minWithdrawInterval = new BN(60); // 60 seconds
    const minClaimDelay = new BN(30); // 30 seconds
    const insuranceFeePercent = new BN(100); // 1%
    const utilizationMultiplier = new BN(100); // 1x multiplier

    // Call the initialize instruction.
    const txHash = await program.methods
      .initialize(
        governance,
        cooldownPeriod,
        earlyWithdrawalPenalty,
        minWithdrawInterval,
        minClaimDelay,
        insuranceFeePercent,
        utilizationMultiplier
      )
      .accounts({
        globalState: globalStateKp.publicKey,
        tokenMint: tokenMint,
        owner: provider.publicKey,
        systemProgram: web3.SystemProgram.programId,
        rent: web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([globalStateKp])
      .rpc();

    console.log(`Initialization transaction: ${txHash}`);

    // Fetch the global state account.
    const globalState = await program.account.globalState.fetch(globalStateKp.publicKey);
    console.log("Global state account:", globalState);

    // Assert that total_staked is 0.
    assert.equal(globalState.totalStaked.toNumber(), 0);
    // (Add additional assertions as desired)
  });
});
*/
