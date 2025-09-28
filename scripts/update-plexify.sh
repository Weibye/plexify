#!/bin/bash
# Plexify Update Script for Linux/macOS
# Downloads and installs the latest release of plexify

set -e

REPO="Weibye/plexify"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
PLATFORM=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Map architecture names
case $ARCH in
  x86_64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) 
    echo -e "${RED}Error: Unsupported architecture: $ARCH${NC}"
    echo "Supported architectures: x86_64, aarch64, arm64"
    exit 1 
    ;;
esac

# Check if we have required tools
if ! command -v curl &> /dev/null; then
    echo -e "${RED}Error: curl is required but not installed${NC}"
    exit 1
fi

if ! command -v sha256sum &> /dev/null; then
    echo -e "${RED}Error: sha256sum is required but not installed${NC}"
    exit 1
fi

echo -e "${BLUE}Plexify Update Script${NC}"
echo "=============================="

# Get current version if plexify is installed
CURRENT_VERSION=""
if command -v plexify &> /dev/null; then
    CURRENT_VERSION=$(plexify --version 2>/dev/null | awk '{print $2}' || echo "unknown")
    echo -e "Current version: ${YELLOW}$CURRENT_VERSION${NC}"
fi

# Get latest release info
echo "Fetching latest release information..."
LATEST_RELEASE=$(curl -s "https://api.github.com/repos/$REPO/releases/latest")

if [ $? -ne 0 ] || [ -z "$LATEST_RELEASE" ]; then
    echo -e "${RED}Error: Failed to fetch release information${NC}"
    exit 1
fi

VERSION=$(echo "$LATEST_RELEASE" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
BINARY_NAME="plexify-${PLATFORM}-${ARCH}"

if [ -z "$VERSION" ]; then
    echo -e "${RED}Error: Could not determine latest version${NC}"
    exit 1
fi

echo -e "Latest version: ${GREEN}$VERSION${NC}"

# Check if we're already up to date
if [ "$CURRENT_VERSION" = "${VERSION#v}" ]; then
    echo -e "${GREEN}You already have the latest version installed!${NC}"
    exit 0
fi

echo "Downloading plexify $VERSION for $PLATFORM-$ARCH..."

# Create temporary directory
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Download binary and checksum
echo "Downloading binary..."
curl -L --fail -o "$TEMP_DIR/$BINARY_NAME" \
  "https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME"

if [ $? -ne 0 ]; then
    echo -e "${RED}Error: Failed to download binary${NC}"
    exit 1
fi

echo "Downloading checksum..."
curl -L --fail -o "$TEMP_DIR/$BINARY_NAME.sha256" \
  "https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME.sha256"

if [ $? -ne 0 ]; then
    echo -e "${RED}Error: Failed to download checksum${NC}"
    exit 1
fi

# Verify checksum
echo "Verifying checksum..."
cd "$TEMP_DIR" && sha256sum -c "$BINARY_NAME.sha256" --quiet

if [ $? -ne 0 ]; then
    echo -e "${RED}Error: Checksum verification failed${NC}"
    exit 1
fi

echo -e "${GREEN}Checksum verified successfully${NC}"

# Make binary executable
chmod +x "$TEMP_DIR/$BINARY_NAME"

# Install binary
echo "Installing to $INSTALL_DIR..."

# Check if we need sudo
if [ -w "$INSTALL_DIR" ]; then
    mv "$TEMP_DIR/$BINARY_NAME" "$INSTALL_DIR/plexify"
else
    echo "Installing with sudo (directory requires elevated permissions)..."
    sudo mv "$TEMP_DIR/$BINARY_NAME" "$INSTALL_DIR/plexify"
fi

if [ $? -ne 0 ]; then
    echo -e "${RED}Error: Failed to install binary${NC}"
    exit 1
fi

echo -e "${GREEN}Successfully updated plexify to $VERSION${NC}"

# Verify installation
if command -v plexify &> /dev/null; then
    INSTALLED_VERSION=$(plexify --version 2>/dev/null | awk '{print $2}' || echo "unknown")
    if [ "$INSTALLED_VERSION" = "${VERSION#v}" ]; then
        echo -e "${GREEN}Installation verified: plexify $INSTALLED_VERSION${NC}"
    else
        echo -e "${YELLOW}Warning: Version mismatch after installation${NC}"
        echo "Expected: ${VERSION#v}, Got: $INSTALLED_VERSION"
    fi
else
    echo -e "${YELLOW}Warning: plexify not found in PATH${NC}"
    echo "Binary installed at: $INSTALL_DIR/plexify"
fi

echo ""
echo "Update complete! You can now use the updated plexify."