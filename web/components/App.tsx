import React, { useState } from "react";
import { WalletMultiButton } from "@solana/wallet-adapter-react-ui";
import { useWallet } from "@solana/wallet-adapter-react";
import { PublicKey, SystemProgram, Transaction, Connection } from "@solana/web3.js";
import bs58 from "bs58";

const PLATFORM_ESCROW = new PublicKey("Fg6PaFpoGXkYsidMpWxqSWKkT5kHZz3wz7L9cB7iQK9V"); // replace with real

function App() {
  const { publicKey, signMessage, signTransaction } = useWallet();
  const [email, setEmail] = useState("");
  const [isLoggedIn, setIsLoggedIn] = useState(false);

  // --- Email login mock ---
  const handleEmailLogin = () => {
    if (!email) return alert("Enter email");
    // Normally send to backend for auth
    setIsLoggedIn(true);
    alert(`Logged in with email: ${email}`);
  };

  // --- Wallet link / login ---
  const handleWalletLogin = async () => {
    if (!publicKey || !signMessage) return alert("Connect wallet first");
    const message = `Login request at ${Date.now()}`;
    const encodedMessage = new TextEncoder().encode(message);
    const signature = await signMessage(encodedMessage);
    alert(
      `Wallet login success!\nPubkey: ${publicKey.toBase58()}\nSignature: ${bs58.encode(
        signature
      )}`
    );
    setIsLoggedIn(true);
  };

  // --- Fiat payment mock ---
  const handleFiatPayment = () => {
    // Normally redirect to Stripe/Paystack checkout
    alert("Redirecting to fiat payment gateway...");
  };

  // --- Crypto payment (SOL transfer) ---
  const handleCryptoPayment = async () => {
    if (!publicKey || !signTransaction) return alert("Connect wallet first");

    const connection = new Connection("https://api.devnet.solana.com");
    const tx = new Transaction().add(
      SystemProgram.transfer({
        fromPubkey: publicKey,
        toPubkey: PLATFORM_ESCROW,
        lamports: 1000000, // 0.001 SOL
      })
    );

    tx.feePayer = publicKey;
    const { blockhash } = await connection.getLatestBlockhash();
    tx.recentBlockhash = blockhash;

    const signed = await signTransaction(tx);
    const sig = await connection.sendRawTransaction(signed.serialize());
    await connection.confirmTransaction(sig);

    alert(`Crypto payment successful!\nTx: ${sig}`);
  };

  return (
    <div style={{ padding: 20 }}>
      <h1>Ride Hailing Hybrid Login + Payment</h1>

      {!isLoggedIn ? (
        <>
          <h2>Login Options</h2>
          <div style={{ marginBottom: 10 }}>
            <input
              type="email"
              placeholder="Enter email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
            <button onClick={handleEmailLogin}>Login with Email</button>
          </div>

          <div>
            <WalletMultiButton />
            <button onClick={handleWalletLogin}>Login with Wallet</button>
          </div>
        </>
      ) : (
        <>
          <h2>Payment Options</h2>
          <button onClick={handleFiatPayment}>Pay with Card (Fiat)</button>
          <button onClick={handleCryptoPayment}>Pay with Crypto (SOL)</button>
        </>
      )}
    </div>
  );
}

export default App;