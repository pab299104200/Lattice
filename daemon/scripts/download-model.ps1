$ModelDir = Join-Path $PSScriptRoot "..\models"
New-Item -ItemType Directory -Force -Path $ModelDir | Out-Null

Write-Host "Downloading all-MiniLM-L6-v2 ONNX model..."
Invoke-WebRequest -Uri "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx" -OutFile "$ModelDir\model.onnx"

Write-Host "Downloading tokenizer..."
Invoke-WebRequest -Uri "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json" -OutFile "$ModelDir\tokenizer.json"

Write-Host "Done."
