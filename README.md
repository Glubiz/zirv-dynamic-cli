# Zirv CLI

[![Build Status](https://github.com/Glubiz/zirv-dynamic-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/Glubiz/zirv-dynamic-cli/actions)
[![Release](https://img.shields.io/github/v/release/Glubiz/zirv-dynamic-cli)](https://github.com/Glubiz/zirv-dynamic-cli/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

> **Zirv CLI** is a cross-platform command-line interface for developers to automate and streamline workflows with YAML, JSON, or TOML scripts.

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
  - [Initialize a Project](#initialize-a-project)
  - [Running Scripts](#running-scripts)
  - [Passing Parameters](#passing-parameters)
  - [Chaining Scripts](#chaining-scripts)
- [Configuration](#configuration)
  - [Directory Structure](#directory-structure)
  - [Schema Examples](#schema-examples)
- [Shortcuts](#shortcuts)
- [Supported Platforms](#supported-platforms)
- [Contribution](#contribution)
- [License](#license)
- [Contact](#contact)

---

## Features

- **YAML-Driven Scripts**: Define commands in `.zirv/` files with metadata (name, description, params, secrets).  
- **Flexible Options**: Interactive modes, OS filters, failure-handling flags, delays, and secret support.  
- **Modular Workflows**: Chain or nest scripts via `pre` or direct script calls.  
- **Cross-Platform**: Compatible with Windows, macOS, and Linux; script blocks can target specific OS.  
- **Multi-Format**: Supports YAML, JSON, and TOML—extendable to your preferred format.  

---

## Installation

Choose one of the following methods:

### Homebrew (macOS)

```bash
brew tap glubiz/homebrew-tap
brew install zirv
```

### Chocolatey (Windows)

```bash
choco install zirv
```

### Cargo (All Platforms)
  
```bash
# Build from source
cargo build --release
# Add `target/release` to your PATH
```

### Precompiled Binaries
Download the latest release from the [GitHub Releases]:
https://github.com/Glubiz/zirv-dynamic-cli/releases

## Usage

### Initialize a Project

Run:
```bash
zirv init
```
Creates a `.zirv/` directory with a sample script. This directory is where you will define your scripts. The .zirv directory is created in the current working directory or in the HOME directory depending on the commandline interactions.

### Running Scripts
Place your script files in .zirv/ (e.g., build.yaml):
  
```yaml
name: Build
description: Build the application.
commands:
  - command: cargo build --release
    options:
      proceed_on_failure: false
  - command: cargo test
    options:
      proceed_on_failure: false
```

Execute the script with:
```bash
zirv build
```

### Passing Parameters
If a script declares parameters;

```yaml
name: Commit Changes
params:
  - commit_message
commands:
  - command: git add .
  - command: git commit -m "${commit_message}"
  - command: git push origin
```

Run with:
```bash
zirv commit "Your commit message here"
```

### Chaining Scripts
You can chain scripts by calling one script from another. For example, if you have a script `build.yaml` and want to call it from `deploy.yaml`:

```yaml
name: Deploy
description: Deploy the application.
commands:
  - command: zirv test
    options:
      proceed_on_failure: false
  - command: zirv build
    options:
      proceed_on_failure: false
```

Run the `deploy` script with:
```bash
zirv deploy
```

## Configuration
### Directory Structure
The `.zirv/` directory contains your scripts and a configuration file. The structure is as follows:

```
.zirv/
├── .shortcuts.yaml
├── ...command files
```

### Schema Examples
Supported schemas are YAML, JSON, and TOML. Below are examples of each:

#### YAML Example
```yaml
name: Example Config
description: An example script.
params:
  - param1
commands:
  - command: echo 'Hello World'
    description: Prints greeting
    options:
      interactive: true
      operating_system: linux
      proceed_on_failure: false
      delay_ms: 2000
secrets:
  - name: api_key
    env_var: API_KEY
```

#### JSON Example
```json
{
  "name": "Example Config",
  "description": "An example script.",
  "params": ["param1"],
  "commands": [
    {
      "command": "echo 'Hello World'",
      "description": "Prints greeting",
      "options": {
        "interactive": true,
        "operating_system": "linux",
        "proceed_on_failure": false,
        "delay_ms": 2000
      }
    }
  ],
  "secrets": [
    {
      "name": "api_key",
      "env_var": "API_KEY"
    }
  ]
}
```

#### TOML Example
```toml
name = "Example Config"
description = "An example script."
params = ["param1"]

[[commands]]
command = "echo 'Hello World'"
description = "Prints greeting"
options.interactive = true
options.operating_system = "linux"
options.proceed_on_failure = false
options.delay_ms = 2000

[[secrets]]
name = "api"
env_var = "API_KEY"
```

## Shortcuts
Shortcuts are defined in `.shortcuts.yaml` and allow you to create aliases for your scripts. For example:

```yaml
shortcuts:
  b: build.yaml
  t: test.yaml
  c: commit.yaml
```
Run zirv b instead of zirv build.yaml.
This will execute the `build.yaml` script.

## Supported Platforms
- Windows
- macOS
- Linux

Commands can target specific operating systems using the `operating_system` option in the script configuration.
- `windows`: Windows OS
- `linux`: Linux OS
- `macos`: macOS

## Contribution
Contributions are welcome! Please fork the repository and submit a pull request with your changes. For major changes, please open an issue first to discuss what you would like to change.

## License
Licensed under the [MIT License](LICENSE).

## Contact
Tweet @ [@Glubiz](https://twitter.com/Glubiz)