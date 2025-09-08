#!/usr/bin/env bash
set -e
echo "Suggested setup commands (run manually):"

echo ""
echo "1) Install root dependencies (npm):"
echo "   npm install"

echo ""
echo "2) Web (Next.js + Tailwind) - install:"
echo "   cd web"
echo "   npm install"
echo "   npm run dev"

echo ""
echo "3) Mobile (Expo) - install & start:"
echo "   cd ../mobile"
echo "   npm install"
echo "   expo start"

echo ""
echo "4) Anchor program:"
echo "   # Install Rust, Solana toolchain, Anchor"
echo "   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
echo "   rustup default stable"
echo "   # Install Solana toolchain (follow docs)"
echo "   # Install Anchor CLI"
echo "   cargo install --git https://github.com/coral-xyz/anchor anchor-cli --locked"

echo ""
echo "This script is a guide — follow official docs for each tool."
