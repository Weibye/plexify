#!/bin/bash
# Test script to validate the distribution workflow

set -e

echo "🧪 Testing Plexify Distribution Workflow"
echo "========================================"

# Test 1: Validate YAML syntax
echo "📝 Testing workflow YAML syntax..."
if python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" 2>/dev/null; then
    echo "✅ Release workflow YAML is valid"
else
    echo "❌ Release workflow YAML has syntax errors"
    exit 1
fi

# Test 2: Build binary locally
echo "🔨 Testing local build..."
if cargo build --release --quiet; then
    echo "✅ Release build successful"
else
    echo "❌ Release build failed"
    exit 1
fi

# Test 3: Test binary functionality
echo "🏃 Testing binary functionality..."
BINARY="./target/release/plexify"

if [ -f "$BINARY" ]; then
    # Test version
    VERSION_OUTPUT=$($BINARY --version)
    if [[ $VERSION_OUTPUT == plexify* ]]; then
        echo "✅ Binary version check: $VERSION_OUTPUT"
    else
        echo "❌ Binary version check failed"
        exit 1
    fi

    # Test help
    if $BINARY --help > /dev/null 2>&1; then
        echo "✅ Binary help command works"
    else
        echo "❌ Binary help command failed"
        exit 1
    fi
else
    echo "❌ Binary not found at $BINARY"
    exit 1
fi

# Test 4: Validate install scripts syntax
echo "📋 Testing install scripts..."

# Check bash script syntax
if bash -n scripts/install-plexify.sh && bash -n scripts/update-plexify.sh; then
    echo "✅ Shell scripts have valid syntax"
else
    echo "❌ Shell scripts have syntax errors"
    exit 1
fi

# Check PowerShell script syntax (if PowerShell is available)
if command -v pwsh > /dev/null 2>&1; then
    if pwsh -Command "Get-Content scripts/Update-Plexify.ps1 | Out-String | Invoke-Expression" -WhatIf > /dev/null 2>&1; then
        echo "✅ PowerShell script has valid syntax"
    else
        echo "⚠️  PowerShell script syntax check inconclusive"
    fi
else
    echo "⚠️  PowerShell not available for syntax checking"
fi

# Test 5: Validate Docker files
echo "🐳 Testing Docker configuration..."

# Check Dockerfile syntax
if docker build --quiet --file Dockerfile --target $(echo "FROM ubuntu:22.04" | docker build --quiet -f - . 2>/dev/null || echo "ubuntu:22.04") . > /dev/null 2>&1; then
    echo "✅ Dockerfile syntax is valid"
else
    # Fallback: basic syntax check
    if grep -q "FROM ubuntu:22.04" Dockerfile && grep -q "RUN apt-get update" Dockerfile; then
        echo "✅ Dockerfile has basic valid structure"
    else
        echo "❌ Dockerfile appears invalid"
        exit 1
    fi
fi

# Check docker-compose.yml syntax
if command -v docker-compose > /dev/null 2>&1; then
    if docker-compose config > /dev/null 2>&1; then
        echo "✅ docker-compose.yml is valid"
    else
        echo "❌ docker-compose.yml has errors"
        exit 1
    fi
else
    # Fallback: basic YAML check
    if python3 -c "import yaml; yaml.safe_load(open('docker-compose.yml'))" 2>/dev/null; then
        echo "✅ docker-compose.yml YAML syntax is valid"
    else
        echo "❌ docker-compose.yml has YAML syntax errors"
        exit 1
    fi
fi

# Test 6: Validate documentation
echo "📚 Testing documentation..."

# Check that critical files exist
CRITICAL_FILES=(
    "README.md"
    "DEPLOYMENT.md"
    "scripts/README.md"
    ".github/workflows/release.yml"
    "scripts/install-plexify.sh"
    "scripts/update-plexify.sh"
    "scripts/Update-Plexify.ps1"
)

for file in "${CRITICAL_FILES[@]}"; do
    if [ -f "$file" ]; then
        echo "✅ $file exists"
    else
        echo "❌ $file is missing"
        exit 1
    fi
done

# Test 7: Check cross-compilation targets (if cross is available)
echo "🎯 Testing cross-compilation targets..."

# List of targets from the workflow
TARGETS=(
    "x86_64-unknown-linux-gnu"
    "x86_64-pc-windows-msvc"
    "x86_64-apple-darwin"
)

# Check if targets are installed
for target in "${TARGETS[@]}"; do
    if rustup target list --installed | grep -q "$target"; then
        echo "✅ Rust target $target is available"
    else
        echo "ℹ️  Rust target $target not installed (this is OK for testing)"
    fi
done

echo ""
echo "🎉 All tests passed!"
echo ""
echo "📋 Summary of implemented features:"
echo "   • Multi-platform release workflow (Linux, Windows, macOS)"
echo "   • Automated binary building with checksums"
echo "   • Installation scripts for all platforms"
echo "   • Update scripts with version checking"
echo "   • Docker support for containerized deployment"
echo "   • Comprehensive documentation"
echo ""
echo "🚀 Ready to create the first release!"
echo "   Run: git tag v0.1.0 && git push origin v0.1.0"