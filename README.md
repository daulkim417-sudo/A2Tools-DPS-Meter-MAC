# A2Tools DPS Meter

⚠️ Work In Progress (WIP)
이 프로젝트는 원작자 taengu 님의 훌륭한 오픈소스 프로젝트를 macOS 환경에서 사용할 수 있도록 이식(Porting) 중인 버전입니다. 원작의 뛰어난 기능을 macOS와 PlayCover 환경에서 즐기기 위해 작업하고 있습니다. 개발의 길을 열어주신 원작자님께 깊은 감사를 드립니다.

[![License](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)

Real-time DPS meter overlay for AION 2. Captures game network packets to display damage, skills, and combat statistics.

**[Download Latest Release](https://github.com/taengu/A2Tools-DPS-Meter/releases)** | **[A2Tools.app](https://a2tools.app)**

## Features

- Real-time DPS tracking with per-player breakdown
- Skill-level damage analysis with crit, back attack, parry, double, and perfect rates
- DOT (damage over time) tracking
- Summon damage merged with owner
- Multiple target selection modes (Boss, Last Hit, All Targets, Train)
- DPS chart and timeline visualization
- Battle history with auto-save for boss fights
- Ping monitoring
- Multi-language support (English, Korean, Chinese Traditional/Simplified)
- Always-on-top transparent overlay
- Themes and customization

## Requirements

- **macOS** (Apple Silicon & Intel)
  - Native support for packet capture via system `libpcap`
  - Requires read permission on `/dev/bpf*` devices

## Installation

npm install && npm run tauri build

## Building from Source

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- [Node.js](https://nodejs.org/) (v18+)

### Build

```bash
npm install
npm run tauri build
```

The MSI installer will be at `src-tauri/target/release/bundle/msi/`.

### Development

```bash
npm run tauri dev
```

## FAQ

**Q: The meter shows "Detecting AION2 connection..."**
A: Make sure AION 2 is running and the app has administrator privileges. If using a VPN or ping reducer, the app will detect the loopback adapter automatically.

**Q: My name doesn't appear on the meter**
A: Enter your character name and actor ID in Settings. The name is auto-detected from the AION 2 window title.

**Q: Npcap is installed but capture doesn't work**
A: Reinstall Npcap and ensure "WinPcap API-compatible Mode" is checked during installation.

## Community

- [Discord](https://discord.gg/Aion2Global)
- [A2Tools.app](https://a2tools.app)

## Support

Say thanks and fund new cool projects & features!

- <img src="wechat.png" width="150">
- ☕ [Buy me a Coffee](https://ko-fi.com/hiddencube)
- ☕ [在爱发电支持我](https://afdian.com/a/hiddencube)
- 🅿️ [Send with PayPal](https://www.paypal.me/taengoo)
- 🎁 [Donate with Crypto](https://nowpayments.io/donation/thehiddencube)
- **BTC**: `1GexKhgVZPYRqpfCKydXLoNUXRRRUoAUwT`
- **ETH**: `0x38F0bc371A563A24eCa6034cFf77eB6173c7e3e7`
- **USDC**: `0xA9571Fc95666350f6DFFB8Fb80ee27eE7db46b56`

## License

[GPL-3.0](LICENSE)
