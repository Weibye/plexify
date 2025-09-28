# Plexify Installation Scripts

This directory contains scripts for installing and updating Plexify on various systems.

## Installation Scripts

### Linux/macOS

**One-line installation:**
```bash
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | bash
```

**Custom installation directory:**
```bash
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/install-plexify.sh | INSTALL_DIR=~/.local/bin bash
```

### Windows

Download and run in PowerShell:
```powershell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/Weibye/plexify/main/scripts/Update-Plexify.ps1" -OutFile "Install-Plexify.ps1"
.\Install-Plexify.ps1
```

## Update Scripts

### Linux/macOS

**Update to latest version:**
```bash
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash
```

**Or download and run locally:**
```bash
wget https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh
chmod +x update-plexify.sh
./update-plexify.sh
```

### Windows

**Update to latest version:**
```powershell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/Weibye/plexify/main/scripts/Update-Plexify.ps1" -OutFile "Update-Plexify.ps1"
.\Update-Plexify.ps1
```

## Automation for Worker Nodes

### Systemd Service (Linux)

Create a systemd service for automated updates:

1. Create service file `/etc/systemd/system/plexify-update.service`:
```ini
[Unit]
Description=Update Plexify to latest version
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
User=plexify
ExecStart=/bin/bash -c 'curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash'
```

2. Create timer file `/etc/systemd/system/plexify-update.timer`:
```ini
[Unit]
Description=Update Plexify daily
Requires=plexify-update.service

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

3. Enable and start:
```bash
sudo systemctl enable plexify-update.timer
sudo systemctl start plexify-update.timer
```

### Cron Job (Linux/macOS)

Add to crontab for automatic updates:
```bash
# Update plexify daily at 2 AM
0 2 * * * curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash >/dev/null 2>&1
```

### Windows Task Scheduler

Create a scheduled task to run the PowerShell update script daily.

## Script Features

### Security Features
- **Checksum verification**: All downloads are verified with SHA256 checksums
- **HTTPS only**: All downloads use secure HTTPS connections
- **Temporary files**: Downloads are done to temporary directories and cleaned up

### User Experience
- **Colored output**: Clear, colored status messages
- **Progress indication**: Shows download and installation progress
- **Error handling**: Comprehensive error checking and reporting
- **Version checking**: Avoids unnecessary downloads if already up-to-date

### Platform Support
- **Linux**: x86_64 and ARM64 architectures
- **macOS**: Intel and Apple Silicon
- **Windows**: x86_64 architecture

## Requirements

### Linux/macOS
- `curl` (for downloading)
- `sha256sum` (for checksum verification)
- Write permissions to installation directory

### Windows
- PowerShell 5.0 or higher
- Internet connection
- Write permissions to installation directory

## Troubleshooting

### Permission Issues
If you get permission errors, either:
1. Choose a different installation directory you can write to
2. Run with appropriate permissions (sudo on Linux/macOS, Administrator on Windows)

### Network Issues
If downloads fail:
1. Check your internet connection
2. Verify GitHub.com is accessible
3. Check if your firewall/proxy is blocking downloads

### Checksum Failures
If checksum verification fails:
1. Try downloading again (may be a temporary network issue)
2. Check if the release files on GitHub are corrupted
3. Report the issue on the GitHub repository