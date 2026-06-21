#!/bin/bash
set -e

echo "========================================"
echo "WaylandWebStream Integration Test"
echo "========================================"
echo ""

# Install Node.js dependencies
echo "Installing Node.js test dependencies..."
cd tests
npm install
cd ..

echo ""
echo "Building compositor and test client..."
cargo build --release

echo ""
echo "Running integration tests..."
cargo test --release -- --nocapture --test-threads=1

echo ""
echo "========================================"
echo "All tests passed!"
echo "========================================"
