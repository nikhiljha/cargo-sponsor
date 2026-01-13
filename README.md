# cargo-sponsor

Find sponsorship links for your Rust project's dependencies.

## Installation

```bash
cargo install cargo-sponsor
```

## Usage

```bash
cargo sponsor
```

This will scan your project's dependencies and display a table of packages that have sponsorship links configured.

### Options

- `--manifest-path <PATH>` - Path to Cargo.toml (default: current directory)
- `--output <FORMAT>` - Output format: `rich` (default) or `json`
- `--top-level-only` - Only show direct dependencies

### GitHub Token

For best results, set a `GITHUB_TOKEN` environment variable or have the GitHub CLI (`gh`) installed and authenticated. This enables fetching sponsor counts and FUNDING.yml information.

## Example Output

```
💝 Sponsorable Dependencies

Found 5 projects you can support:

Package     Sponsors  Link
──────────  ────────  ────────────────────────────────────────
serde       42        https://github.com/sponsors/dtolnay
tokio       128       https://github.com/sponsors/tokio-rs
...
```

## License

MIT
