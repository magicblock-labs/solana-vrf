import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { assert } from "chai";
import { UseRandomness } from "../target/types/use_randomness";

describe("use-randomness", () => {
  // Configure the client to use the local cluster.
  anchor.setProvider(anchor.AnchorProvider.env());

  const program = anchor.workspace.useRandomness as Program<UseRandomness>;

  it("Request randomness", async () => {
    const randomSeed = Math.floor(Math.random() * 256);
    const tx = await program.methods.requestRandomness(randomSeed).rpc();
    console.log("Request randomness", tx);
  });

  it("Simpler request randomness", async () => {
    const randomSeed = Math.floor(Math.random() * 256);
    const tx = await program.methods.simplerRequestRandomness(randomSeed).rpc();
    console.log("Request randomness", tx);
  });

  it("Cheaper request randomness", async () => {
    const randomSeed = Math.floor(Math.random() * 256);
    const tx = await program.methods.cheaperRequestRandomness(randomSeed).rpc();
    console.log("Request randomness", tx);
  });

  // Default (scoped identity) pattern ------------------------------------------------------------

  it("Scoped consume rejects a non-scoped identity", async () => {
    // The `consume_randomness` callback validates the scoped per-program identity PDA
    // (injected by `#[vrf_callback]`). Any other account passed as `vrf_program_identity`
    // must be rejected. We can only sign for a keypair we control, which is not the scoped
    // PDA, so the address constraint must fail.
    const wrongIdentity = anchor.web3.Keypair.generate();
    const randomness = Array.from({ length: 32 }, () => 7);

    let rejected = false;
    try {
      await program.methods
        .consumeRandomness(randomness)
        .accounts({ vrfProgramIdentity: wrongIdentity.publicKey })
        .signers([wrongIdentity])
        .rpc();
    } catch (e) {
      rejected = true;
      console.log("Scoped consume correctly rejected wrong identity");
    }
    assert.ok(
      rejected,
      "consume_randomness must reject a vrf_program_identity that is not the scoped PDA"
    );
  });

  // Backward-compatibility: the deprecated legacy (global identity) path still works -------------

  it("Legacy request randomness (backward compat)", async () => {
    const randomSeed = Math.floor(Math.random() * 256);
    const tx = await program.methods.requestRandomnessLegacy(randomSeed).rpc();
    console.log("Legacy request randomness", tx);
  });

  it("Legacy consume rejects a non-global identity", async () => {
    const wrongIdentity = anchor.web3.Keypair.generate();
    const randomness = Array.from({ length: 32 }, () => 7);

    let rejected = false;
    try {
      await program.methods
        .consumeRandomnessLegacy(randomness)
        .accounts({ vrfProgramIdentity: wrongIdentity.publicKey })
        .signers([wrongIdentity])
        .rpc();
    } catch (e) {
      rejected = true;
    }
    assert.ok(
      rejected,
      "consume_randomness_legacy must reject an identity that is not the global VRF identity"
    );
  });
});
