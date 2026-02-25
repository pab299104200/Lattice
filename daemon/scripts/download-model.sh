#!/bin/bash
# Downloads the all-MiniLM-L6-v2 ONNX model for Lattice embedding engine
set -e

MODEL_DIR="$(dirname "$0")/../models"
mkdir -p "$MODEL_DIR"

echo "Downloading all-MiniLM-L6-v2 ONNX model..."
curl -L -o "$MODEL_DIR/model.onnx" \
  "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx"

echo "Downloading tokenizer..."
curl -L -o "$MODEL_DIR/tokenizer.json" \
  "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json"

echo "Done. Model files saved to $MODEL_DIR/"
