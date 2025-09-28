# Plexify Docker Image for Distributed Workers
FROM ubuntu:22.04

# Install dependencies
RUN apt-get update && apt-get install -y \
    ffmpeg \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create plexify user
RUN useradd -m -s /bin/bash plexify

# Download and install plexify
RUN curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | \
    INSTALL_DIR=/usr/local/bin bash

# Create media mount point
RUN mkdir -p /media && chown plexify:plexify /media

# Switch to plexify user
USER plexify
WORKDIR /home/plexify

# Default command
CMD ["plexify", "--help"]

# Labels
LABEL org.opencontainers.image.title="Plexify"
LABEL org.opencontainers.image.description="Distributed media transcoding CLI tool"
LABEL org.opencontainers.image.url="https://github.com/Weibye/plexify"
LABEL org.opencontainers.image.source="https://github.com/Weibye/plexify"
LABEL org.opencontainers.image.vendor="Plexify"