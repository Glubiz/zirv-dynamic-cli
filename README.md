# zirv

**zirv** is a CLI tool that helps developer teams standardize interactions with their application,
build tools, Git, and more by executing commands defined in YAML files. All YAML files are placed in a 
`.zirv` directory at the project root.

## Features

- **YAML-Driven Scripts:**  
  Define scripts in YAML files (e.g., `build.yaml`, `test.yaml`) in the `.zirv` folder.
  
- **Command Options:**  
  Each command can include additional options:
  - `interactive` (bool): If true, runs the command interactively (ideal for commands like `docker exec -it {id} bash`).
  - `operating_system` (string, optional): Specifies the OS on which the command should run. Commands are only executed if this value matches the current OS (e.g., `"linux"`, `"windows"`, `"macos"`).
  - `proceed_on_failure` (bool): If false, a command failure aborts the script; if true, the script continues.
  - `delay_ms` (optional, u64): Delay in milliseconds after the command finishes.
  
- **Chaining:**  
  YAML files can chain other YAML files using the `pre` field, allowing you to run pre‑build or pre‑deployment steps.

- **Extensibility:**  
  Easily extend the YAML schema with additional options as your needs evolve.

## Usage

1. Create a `.zirv` directory at the root of your project and in you home folder. Alternatively you could call `zirv init` to create it automatically.

`zirv init` will create the following structure:

```
.zirv
├── .shortcuts.yaml
```

2. Add YAML files by calling `zirv create` to define your commands. For example:

### Example: `build.yaml`

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

### Example: `deploy.yaml`

```yaml
name: Deploy
description: Deploy the application to the server.
commands:
  - command: scp target/release/app user@server:/path/to/deploy
    options:
      proceed_on_failure: false
      operating_system: linux
```

### Example: `commit.yaml`

```yaml
name: "Commit Changes"
description: "Commits changes with a provided commit message"
params:
  - "commit_message"
commands:
  - command: "git add ."
    description: "Stage all changes"
    options:
      proceed_on_failure: false
  - command: "git commit -m \"${commit_message}\""
    description: "Commit changes with a message"
    options:
      proceed_on_failure: false
  - command: "git push origin"
    description: "Push the commit to the remote repository"
    options:
      proceed_on_failure: false
```

## Example: `secret.yaml`

```yaml
name: "Secret Command"
description: "This command is secret"
secrets:
  - name: "some_secret"
    env_var: "SOME_SECRET"
commands:
  - command: "echo 'This is a secret ${some_secret}'"
```

3. Run the commands using **zirv**:

```bash
zirv build
zirv deploy
```

4. Pass parameters to the script:

```bash
zirv commit "Fix bug #123"
```

5. Chain scripts together:

```yaml
name: "Build and Deploy"
description: "Builds the application and deploys it to the server"
commands:
    - command: "zirv build"
    - command: "deploy"
        options:
        proceed_on_failure: false
```

6. Shortcuts:
By adding a file named `.shortcuts.yaml` to the `.zirv` directory, you can define shortcuts for your scripts:

```yaml
shortcuts:
  t: "test.yaml"
  b: "build.yaml"
  c: "commit.yaml"
```

Now you can run `zirv t` to run the `test.yaml` script.

## Configuration Format Examples

The configuration for **zirv** can be defined in YAML, JSON, or TOML formats. The structure is based on our configuration schema from main.rs:

**Common Fields:**
- name (string)
- description (string)
- params (optional list of strings)
- commands (list)
  - command (string)
  - description (optional string)
  - options:
    - interactive (optional bool)
    - operating_system (optional string)
    - proceed_on_failure (bool)
    - delay_ms (optional number)
- secrets (optional list)
  - name (string)
  - env_var (string)

### YAML Example
```yaml
name: "Example Config"
description: "This is an example configuration."
params:
  - "param1"
commands:
  - command: "echo 'Hello World'"
    description: "Prints Hello World"
    options:
      interactive: true
      operating_system: "linux"
      proceed_on_failure: false
      delay_ms: 2000
secrets:
  - name: "api_key"
    env_var: "API_KEY"
```

### JSON Example
```json
{
  "name": "Example Config",
  "description": "This is an example configuration.",
  "params": [
    "param1"
  ],
  "commands": [
    {
      "command": "echo 'Hello World'",
      "description": "Prints Hello World",
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

### TOML Example
```toml
name = "Example Config"
description = "This is an example configuration."
params = ["param1"]

[[commands]]
command = "echo 'Hello World'"
description = "Prints Hello World"
[commands.options]
interactive = true
operating_system = "linux"
proceed_on_failure = false
delay_ms = 2000

[[secrets]]
name = "api_key"
env_var = "API_KEY"
```

## Help

```bash
zirv help
```

## Installation

Build and install **zirv** using Cargo:

```bash
cargo build --release
```

Then add the resulting binary to your PATH.

**Alternatively, you can download precompiled executables from our GitHub Releases page: [GitHub Releases](https://github.com/Glubiz/zirv-dynamic-cli/releases).**