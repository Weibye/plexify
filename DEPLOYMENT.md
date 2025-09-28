# Plexify Deployment Guide

This document describes the deployment and distribution strategy for Plexify.

## Release Process

### Automated Releases

Releases are automatically created when a Git tag is pushed:

```bash
# Tag and push a new release
git tag v0.2.0
git push origin v0.2.0
```

This triggers the GitHub Actions release workflow that:

1. **Builds binaries** for multiple platforms:
   - Linux (x86_64, ARM64)
   - Windows (x86_64)
   - macOS (Intel, Apple Silicon)

2. **Creates checksums** for all binaries using SHA256

3. **Creates a GitHub release** with:
   - Release notes
   - Binary artifacts
   - Installation instructions
   - Checksum files

### Manual Release (if needed)

You can also trigger a release manually:

1. Go to GitHub Actions → Release workflow
2. Click "Run workflow"
3. Enter the tag name (e.g., `v0.2.0`)
4. Click "Run workflow"

## Distribution Channels

### 1. GitHub Releases (Primary)

- **URL**: https://github.com/Weibye/plexify/releases
- **Artifacts**: Pre-built binaries for all platforms
- **Checksums**: SHA256 checksums for verification
- **Automation**: Fully automated via GitHub Actions

### 2. Installation Scripts

#### One-line Installation
```bash
# Linux/macOS
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | bash

# Windows PowerShell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/Weibye/plexify/main/scripts/Update-Plexify.ps1" -OutFile "Install-Plexify.ps1"; .\Install-Plexify.ps1
```

#### Update Scripts
```bash
# Linux/macOS
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash

# Windows PowerShell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/Weibye/plexify/main/scripts/Update-Plexify.ps1" -OutFile "Update-Plexify.ps1"; .\Update-Plexify.ps1
```

### 3. Docker Images

#### Building Local Images
```bash
# Build image
docker build -t plexify:latest .

# Run worker
docker run -v /path/to/media:/media -v /path/to/queue:/queue plexify:latest plexify work /media --queue-dir /queue
```

#### Docker Compose
```bash
# Start worker
docker-compose up -d plexify-worker

# Run scanner
docker-compose run --rm plexify-scanner
```

## Deployment Strategies

### Single Node Deployment

For a single machine with local media:

```bash
# Install plexify
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | bash

# Scan media
plexify scan /path/to/media

# Process jobs
plexify work /path/to/media
```

### Distributed Deployment

For multiple worker nodes with shared storage:

#### Shared Storage Setup
```bash
# On each worker node
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | bash

# Mount shared storage (example with NFS)
sudo mount -t nfs server:/shared/media /media
sudo mount -t nfs server:/shared/queue /queue

# Start worker in background mode
plexify work /media --queue-dir /queue --background
```

#### Docker Swarm Setup
```yaml
# docker-compose.prod.yml
version: '3.8'

services:
  plexify-worker:
    image: plexify:latest
    volumes:
      - media-nfs:/media
      - queue-nfs:/queue
    environment:
      - RUST_LOG=info
    command: ["plexify", "work", "/media", "--queue-dir", "/queue", "--background"]
    deploy:
      replicas: 3
      resources:
        limits:
          cpus: '2.0'
          memory: 2G

volumes:
  media-nfs:
    driver: local
    driver_opts:
      type: nfs
      o: addr=nfs-server,rw
      device: ":/shared/media"
  
  queue-nfs:
    driver: local
    driver_opts:
      type: nfs
      o: addr=nfs-server,rw
      device: ":/shared/queue"
```

### Production Monitoring

#### Systemd Service (Linux)
```ini
# /etc/systemd/system/plexify-worker.service
[Unit]
Description=Plexify Media Transcoding Worker
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=plexify
WorkingDirectory=/home/plexify
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/plexify work /media --queue-dir /queue --background
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

#### Windows Service
Use NSSM (Non-Sucking Service Manager) to run plexify as a Windows service:

```cmd
# Install NSSM
winget install nssm

# Create service
nssm install PlexifyWorker "C:\Program Files\plexify\plexify.exe"
nssm set PlexifyWorker Arguments "work C:\Media --queue-dir C:\Queue --background"
nssm set PlexifyWorker DisplayName "Plexify Media Transcoding Worker"
nssm set PlexifyWorker Description "Distributed media transcoding worker"
nssm set PlexifyWorker Start SERVICE_AUTO_START

# Start service
nssm start PlexifyWorker
```

## Update Strategies

### Automated Updates

#### Cron-based Updates (Linux/macOS)
```bash
# Add to crontab
0 2 * * * curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash && systemctl restart plexify-worker
```

#### Windows Task Scheduler
1. Create a new task in Task Scheduler
2. Set trigger: Daily at 2 AM
3. Set action: Run PowerShell script
4. Script: Update-Plexify.ps1 followed by service restart

#### Docker Updates
```bash
# Update and restart
docker-compose pull && docker-compose up -d
```

### Rolling Updates

For zero-downtime updates in distributed setups:

```bash
#!/bin/bash
# rolling-update.sh

NODES=("worker1" "worker2" "worker3")

for node in "${NODES[@]}"; do
    echo "Updating $node..."
    ssh $node "curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash"
    ssh $node "systemctl restart plexify-worker"
    echo "Waiting for $node to stabilize..."
    sleep 30
done

echo "Rolling update complete!"
```

## Monitoring and Logging

### Log Aggregation
```bash
# Configure structured logging
export RUST_LOG=plexify=info

# Forward logs to syslog
plexify work /media 2>&1 | logger -t plexify
```

### Health Checks
```bash
#!/bin/bash
# health-check.sh

# Check if plexify is running
if pgrep -f "plexify work" > /dev/null; then
    echo "✅ Plexify worker is running"
else
    echo "❌ Plexify worker is not running"
    exit 1
fi

# Check if media directory is accessible
if [ -d "/media" ]; then
    echo "✅ Media directory is accessible"
else
    echo "❌ Media directory not found"
    exit 1
fi

# Check if there are jobs in queue
QUEUE_COUNT=$(find /queue/_queue -name "*.json" 2>/dev/null | wc -l)
echo "📊 Jobs in queue: $QUEUE_COUNT"
```

## Security Considerations

### File Permissions
```bash
# Create dedicated user
sudo useradd -r -s /bin/false plexify

# Set appropriate permissions
sudo chown -R plexify:plexify /queue
sudo chmod 755 /queue
```

### Network Security
- Use VPN for remote worker nodes
- Restrict access to shared storage
- Use TLS for management interfaces

### Update Security
- All downloads use HTTPS
- SHA256 checksums verify integrity
- Scripts can be audited before execution

## Troubleshooting

### Common Issues

1. **Permission Errors**
   ```bash
   # Fix ownership
   sudo chown -R plexify:plexify /media /queue
   ```

2. **Network Issues**
   ```bash
   # Test connectivity
   curl -I https://github.com/Weibye/plexify/releases/latest
   ```

3. **FFmpeg Not Found**
   ```bash
   # Install FFmpeg
   sudo apt install ffmpeg  # Ubuntu/Debian
   brew install ffmpeg      # macOS
   winget install ffmpeg    # Windows
   ```

4. **Disk Space Issues**
   ```bash
   # Clean completed jobs
   plexify clean /media
   ```

### Debug Mode
```bash
# Enable debug logging
RUST_LOG=debug plexify work /media
```

## Performance Tuning

### Resource Limits
```bash
# Set CPU limits (systemd)
echo "CPUQuota=200%" >> /etc/systemd/system/plexify-worker.service

# Set memory limits
echo "MemoryLimit=2G" >> /etc/systemd/system/plexify-worker.service
```

### FFmpeg Optimization
```bash
# Environment variables for performance
export FFMPEG_PRESET=ultrafast  # Faster encoding
export FFMPEG_CRF=28            # Lower quality, smaller files
export SLEEP_INTERVAL=30        # Check for jobs more frequently
```