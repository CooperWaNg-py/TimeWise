# TimeWise - Product Requirements Document

> Version: 1.0 (Meeting 4, 2026-07-19)
> Status: Draft for review

## 1. Product Overview

TimeWise is a screen time tracking tool designed for children under 15 and their parents. Its purpose is to help kids understand how much time they spend on electronic devices, fostering self-awareness and self-regulation through data visibility and positive reinforcement - not punishment or enforcement.

The MVP focuses on **Windows and macOS desktop tracking only**. Future versions will expand to mobile devices, browser-level URL tracking, and potentially other data sources (wearables, activity trackers).

**Tagline:** *Understand your screen time.*

## 2. Target Users

| Role | Description | Key Needs |
|---|---|---|
| **Child (primary subject)** | Under 15 years old, uses Windows/macOS computer | See their own screen time, earn rewards for meeting goals, receive gentle break reminders |
| **Parent (administrator)** | Manages household setup, sets goals, reviews reports | View per-child and per-app dashboards, configure goals/warnings, receive disconnect alerts |

## 3. Core Design Philosophy

- **Passive, reports-only by default** - the tool captures data and presents it; behavior change comes from awareness and family conversation, not enforcement
- **Positive reinforcement over punishment** - reward system incentivises good habits; gentle warnings nudge without locking out
- **Self-management focus** - the tool assists, not controls; the user must willingly participate
- **Simplicity is critical** - kids who lack computer skills will abandon complex software; the interface must be straightforward
- **Control is opt-in only** - hard enforcement (daily time caps, blocking) is a future optional mode, never the default
- **Privacy by default** - all data stays within the household network; no cloud required

## 4. MVP Features

### 4.1 Active Window Tracking
- Monitor the currently active window on Windows and macOS
- Record application name, window title, and duration of focus
- Run as a system service (Windows Service / macOS launch daemon) for tamper resistance
- Near-zero CPU usage when idle; minimal memory footprint

### 4.2 Per-Application Time Recording
- Track time spent in each application
- Session-based: record start time and end time per active window
- Duration calculated and stored in seconds

### 4.3 App Categorization (Layered)
- **Layer 1:** Bundled static lookup table (seeded from `software-catalog` npm package + common kids' apps)
- **Layer 2:** Regex-based rules for pattern matching (e.g., browser titles)
- **Layer 3:** Parent override UI to manually categorise any application
- Categories include: Games, Educational, Entertainment, Social Media, Productivity, Browsers, Other

### 4.4 Data Sync to Parent Server
- Agent buffers data locally for offline resilience
- Batched HTTP POST to parent server every ~60 seconds
- JSON payloads containing session records
- Exponential backoff retry if server is unreachable

### 4.5 Parent Dashboard (Web-Based)
- Per-child total screen time (daily, weekly)
- Per-application breakdown with time and percentage
- Time-of-day distribution (morning/afternoon/evening)
- Agent connection status (online/disconnected via heartbeat monitoring)
- Accessible from any modern browser

### 4.6 Goal Setting & Points-Based Rewards
- Parents set daily and/or weekly screen time goals per child
- Child earns points for staying within goals
- Points redeemable for real-world rewards provided by parents (design details TBD)

### 4.7 Gentle Warning Escalation Ladder
- Configurable thresholds (e.g., at 90% of goal → nudge, at 100% → reminder, at 110% → suggestion to take a break)
- Non-blocking notifications only - no lockout in default mode
- Positive framing (e.g., "Great job staying close to your goal!" or "You've been at it a while, maybe take a stretch break")

### 4.8 Screen Break Prompts
- After configurable continuous usage (e.g., 30-45 minutes)
- Non-blocking notification suggesting the child stand up, stretch, or rest eyes
- Aligns with the philosophy of awareness, not enforcement

## 5. Architecture

### 5.1 Components

```
┌──────────────────────┐       ┌──────────────────────────┐
│   Agent (per child)   │       │   Parent Server           │
│ ┌──────────────────┐  │ HTTP  │ ┌──────────────────────┐  │
│ │ Window Tracker    │──┼──────┼─▶│  REST API            │  │
│ │ App Categorizer   │  │       │ │  (FastAPI/Python)    │  │
│ │ Local Buffer      │  │       │ ├──────────────────────┤  │
│ │ Heartbeat Sender  │  │       │ │  SQLite Database      │  │
│ │ System Service    │  │       │ ├──────────────────────┤  │
│ └──────────────────┘  │       │ │  Web Dashboard        │  │
│ Tauri 2 (Rust)        │       │ │  (Vue.js + Chart.js)   │  │
└──────────────────────┘       │ └──────────────────────┘  │
                                │ Self-hosted (parent       │
                                │ machine / Raspberry Pi)   │
                                └──────────────────────────┘
```

### 5.2 Data Flow
1. Agent detects active window change → records start time and application info
2. Agent buffers data locally for resilience
3. Every ~60 seconds, agent sends batched data to parent server via HTTP POST
4. Server receives, validates, and stores data in SQLite
5. If server is unreachable, agent retries with exponential backoff
6. Parent dashboard queries server database to render charts and reports

### 5.3 Technology Stack

| Component | Technology |
|---|---|
| Agent framework | Tauri 2 (Rust backend + web frontend for system tray) |
| Agent UI | System tray menu, configuration window |
| Server backend | Python (FastAPI) |
| Server frontend | Vue.js |
| Charts | Chart.js or D3 |
| Database | SQLite (MVP) |
| Communication | HTTP REST API, JSON payloads |

## 6. Non-Functional Requirements

| # | Requirement |
|---|---|
| NFR1 | **Minimal resource usage** - Agent must use near-zero CPU when idle and minimal RAM (many family computers are old/underpowered) |
| NFR2 | **Privacy by default** - All data stays within the household network; no external cloud required for MVP |
| NFR3 | **Tamper resistance** - Agent runs as a system service (Windows Service / macOS launch daemon) requiring admin privileges to stop; child with standard user account cannot interfere |
| NFR4 | **Heartbeat monitoring** - Agent sends periodic heartbeats to server; if heartbeats stop, dashboard marks agent as disconnected and alerts parent |
| NFR5 | **Cross-browser dashboard** - Parent dashboard must work on any modern browser (Chrome, Firefox, Safari, Edge) |
| NFR6 | **Multi-child / multi-computer support** - System supports multiple children per household and multiple computers per child |
| NFR7 | **Offline resilience** - Agent buffers data locally when server is unavailable; syncs automatically on reconnection |

## 7. Out of MVP Scope (Post-MVP)

These features are explicitly deferred for later releases:

| Priority | Feature | Notes |
|---|---|---|
| High | Mobile device tracking (Android/iOS) | Cross-device vision; deferred due to shared-device and OS restriction complexity |
| High | Browser extension for URL-level tracking | E.g., YouTube music vs. YouTube educational classification; requires browser extension architecture |
| Medium | Printable weekly report card | Physical paper for fridge; sparks family conversation |
| Medium | Kid's simplified view | Streamlined dashboard for kids with progress bars, streak counters, fun facts |
| Medium | Focus mode detection | Auto-tagging educational vs. entertainment activity (beyond basic categorization) |
| Low | Parent-child contract feature | Shared goal setting between parent and child |
| Low | Goal templates based on AAP guidelines | American Academy of Pediatrics age-based screen time recommendations |
| Low | Cloud sync option | Remote access and cross-source data aggregation |
| Low | Hard control / enforcement mode | Daily time caps with enforced limits; opt-in only, never default |

## 8. Open Questions

1. **Reward mechanics** - How many points per goal met? How do parents define and manage redemptions?
2. **Setup experience** - How does the parent configure the server (one-click installer? Docker? Manual Python setup?) and install agents on child computers?
3. **Hard control mechanism** - If opted in, how is enforcement achieved (network-level? OS-level? Agent-level?)?
4. **Browser extension architecture** - How does the extension communicate with the agent? Does it run as a separate component?
5. **Shared computer handling** - Multiple children using one computer via separate OS accounts: how does the agent know which child is active?

## 9. Glossary

| Term | Definition |
|---|---|
| Agent | Lightweight Tauri 2 application running as a system service on the child's computer |
| Server | Self-hosted web application on the parent's machine that receives and stores tracking data |
| MVP | Minimum Viable Product - the smallest useful version for initial release |
| AAP | American Academy of Pediatrics - publishes age-based screen time guidelines |
| Heartbeat | Periodic signal from agent to server indicating the agent is running and connected |
