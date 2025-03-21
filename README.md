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

1. Create a `.zirv` directory at the root of your project.
2. Add YAML files to define your commands. For example:

### Example: `build.yaml`

```yaml
commands:
  - name: compile
    run: cargo build --release
    proceed_on_failure: false
  - name: test
    run: cargo test
    delay_ms: 500
```

### Example: `deploy.yaml`

```yaml
pre: build.yaml
commands:
  - name: deploy
    run: scp target/release/app user@server:/path/to/deploy
    operating_system: linux
```

3. Run the commands using **zirv**:

```bash
zirv run build.yaml
zirv run deploy.yaml
```

## Installation

Build and install **zirv** using Cargo:

```bash
cargo build --release
```

Then add the resulting binary to your PATH.