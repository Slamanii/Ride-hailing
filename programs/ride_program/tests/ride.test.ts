// Minimal Anchor test (TypeScript)
import * as anchor from "@project-serum/anchor";
import { Program } from "@project-serum/anchor";

describe("ride_program", () => {
  // Configure the client to use the local cluster.
  anchor.setProvider(anchor.AnchorProvider.env());
  const program = anchor.workspace.RideProgram as Program;

  it("Initializes program", async () => {
    // simple sanity test
    const tx = await program.rpc.initialize();
    console.log("tx:", tx);
  });
});
