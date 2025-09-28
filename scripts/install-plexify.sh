#!/bin/bash
# Plexify Installation Script for Linux/macOS
# Downloads and installs the latest release of plexify

set -e

REPO="Weibye/plexify"
DEFAULT_INSTALL_DIR="/usr/local/bin"
PLATFORM=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Show usage
show_usage() {
    echo "Plexify Installation Script"
    echo ""
    echo "USAGE:"
    echo "  curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | bash"
    echo "  curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | INSTALL_DIR=/usr/local/bin bash"
    echo ""
    echo "ENVIRONMENT VARIABLES:"
    echo "  INSTALL_DIR    Directory to install plexify (default: $DEFAULT_INSTALL_DIR)"
    echo ""
    echo "EXAMPLES:"
    echo "  # Install to default location"
    echo "  curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | bash"
    echo ""
    echo "  # Install to custom location"
    echo "  curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | INSTALL_DIR=~/.local/bin bash"
}

# Check for help flag
if [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    show_usage
    exit 0
fi

INSTALL_DIR="${INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"

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

echo -e "${BLUE}Plexify Installation Script${NC}"
echo "==============================="
echo -e "Platform: ${YELLOW}$PLATFORM-$ARCH${NC}"
echo -e "Install directory: ${YELLOW}$INSTALL_DIR${NC}"
echo ""

# Check if plexify is already installed
if command -v plexify &> /dev/null; then
    CURRENT_VERSION=$(plexify --version 2>/dev/null | awk '{print $2}' || echo "unknown")
    echo -e "${YELLOW}Warning: plexify is already installed (version $CURRENT_VERSION)${NC}"
    echo "This will overwrite the existing installation."
    echo ""
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
echo ""

# Create temporary directory
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Download binary and checksum
echo "Downloading plexify $VERSION for $PLATFORM-$ARCH..."
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

# Create install directory if it doesn't exist
if [ ! -d "$INSTALL_DIR" ]; then
    echo "Creating install directory..."
    if mkdir -p "$INSTALL_DIR" 2>/dev/null; then
        echo "Created directory: $INSTALL_DIR"
    else
        echo "Creating directory with sudo..."
        sudo mkdir -p "$INSTALL_DIR"
    fi
fi

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

echo -e "${GREEN}Successfully installed plexify $VERSION${NC}"

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
    echo "You may need to add $INSTALL_DIR to your PATH"
fi

echo ""
echo -e "${GREEN}Installation complete!${NC}"
echo ""
echo "Next steps:"
echo "1. Install FFmpeg on your system if not already installed"
echo "2. Try: plexify --help"
echo "3. Start with: plexify scan /path/to/your/media"
echo ""
echo "For updates, you can use the update script:"
echo "curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash"