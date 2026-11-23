# Dark Souls III freeze tools

> **WARNING: freezefix modifies Dark Souls III in memory while the game is running. If you play ONLINE with it,
> FromSoftware's anti-cheat can detect a modified game and soft-ban your account: you are moved to a separate
> "cheater" matchmaking pool, often permanently. Use it ONLY in Steam Offline Mode, and never while connected to
> FromSoftware's servers. Use it at your own risk.**

The unmodified Dark Souls III 1.15.2.0 has a periodic stutter. During normal gameplay a frame takes about 150 ms
(8 missed frames at 60 FPS) every 30 seconds or so. The cause is the integrity checks of the game's copy
protection: they hash large memory regions on the render thread.

| Tool | What it does |
|------|--------------|
| [freezemon](freezemon/) | External freeze monitor. Reads DXGI Present and CPU sampling events through ETW; injects nothing into the game. Logs every long frame and where the game spent that time. Use it to measure before and after the fix. |
| [freezefix](freezefix/) | Runtime fix DLL, installed as the game's `dinput8.dll` (or loaded by ModEngine2). Applies a few small in-memory patches that skip the two stalling hash loops, re-applies them whenever the protection reverts them, and blocks all network access of the game. |

## Compatibility

- Steam `DarkSoulsIII.exe` **1.15.2.0** only. On any other version freezefix writes a line to its log and patches
  nothing.
- Installation: copy `freezefix.dll` into the game folder as `dinput8.dll`; no mod loader is needed. ModEngine2
  2.1.0.0 or newer works as well.
- While the DLL is loaded, the game has no network access at all (built in, cannot be turned off). This is a
  safety net only: still use Steam Offline Mode.
- The game files on disk are never modified. The copy protection stays fully active and keeps restoring all its
  other hooks; freezefix touches only its own patch sites.

## Repository layout

```
freezefix/  src/  bin/ (freezefix.dll)  README.md
freezemon/  src/  bin/ (freezemon.exe)  README.md
```

## Verifying the binaries (reproducible build)

The binaries in `bin/` can be rebuilt byte for byte from this source, so you can check that they contain nothing
else.

| File | SHA-256 |
|------|---------|
| `freezefix/bin/freezefix.dll` | `affd7df1d93367cc79bfc083c66b7c798c0fccb8bc79712d0011fd1d626e4b95` |
| `freezemon/bin/freezemon.exe` | `cb11bc920925611b03b4c683713aaa6eb3c2b66661685fda1bdfed06a849e193` |

Toolchain used for the published binaries:

- Rust **1.99.0** (`x86_64-pc-windows-msvc`, LLVM 23.1.1), pinned by `rust-toolchain.toml`; rustup installs it
  automatically.
- Visual Studio 2022 Build Tools, MSVC toolset **14.44.35207** (`link.exe` 14.44.35229.0), Windows SDK.
- Dependencies pinned by each `Cargo.lock` (`windows-sys` 0.61.2, `serde_json` 1.0.151, `sha2` 0.10.9).

Build one crate in PowerShell (shown for `freezefix`; for `freezemon` use its folder and `freezemon.exe`):

```powershell
git clone <this repository>
cd <repository>\freezefix
Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { "$env:USERPROFILE\.cargo" }
$env:CARGO_ENCODED_RUSTFLAGS = @(
    "--remap-path-prefix=$((Get-Location).Path)=."
    "--remap-path-prefix=$cargoHome=~/.cargo"
    "-Clink-arg=/Brepro"
) -join [char]0x1f
$env:CARGO_INCREMENTAL = "0"
cargo build --release --locked
Get-FileHash target\release\freezefix.dll, bin\freezefix.dll
```

Both hashes must be equal to the value in the table above.

What these settings do:

- `--remap-path-prefix` replaces the absolute paths of the crate and of the cargo registry (which would otherwise
  appear in panic messages, including your user name) with `.` and `~/.cargo`. `CARGO_ENCODED_RUSTFLAGS` is used
  instead of `RUSTFLAGS` so that paths with spaces stay intact.
- `/Brepro` makes the MSVC linker write a content hash instead of the build time into the PE timestamp and the
  debug directory.
- `.gitattributes` forces LF line endings: `bodies.json` is embedded into the DLL and line numbers end up in panic
  locations, so a CRLF checkout would change the binary.
- Release profile: `lto = "fat"`, `codegen-units = 1`; `CARGO_INCREMENTAL=0`; `cargo build --locked`.

Checked: building from a different directory (with a space in its path) and with a different `CARGO_HOME` gives
identical files. Not guaranteed: a different MSVC linker version can produce a different file (the linker version
is written into the PE header, and layout may change). If the hashes differ, check your `link.exe` version first.

## CI

`.github/workflows/ci.yml` runs on `windows-latest` for both crates: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo build --release --locked` and `cargo test`.

## Disclaimer

These are unofficial third-party tools, not affiliated with or endorsed by FromSoftware or Bandai Namco
Entertainment. DARK SOULS is a trademark or registered trademark of Bandai Namco Entertainment Inc.

## License

[MIT](LICENSE)
