# freezemon

External freeze monitor for Dark Souls III. It injects nothing into the game and does not touch its files.

- Frames: DXGI `Present` events (ETW). A **freeze** is a gap between two presents longer than `--threshold` frame
  periods (default 1.5 frames at 60 FPS, i.e. 25 ms).
- CPU: the kernel sampled profile (ETW, default every 125 µs). For each freeze, the samples of the game's threads
  inside the gap are grouped into zones: the game's code sections, other modules (by nearest export), and the
  kernel.

It finds any running `DarkSoulsIII*.exe` by itself, so it works for the original game and for modified copies.

## Run

1. Open a terminal **as Administrator** (kernel ETW tracing requires it).
2. Run `bin\freezemon.exe`. It can be started before or after the game; it waits for `DarkSoulsIII*.exe`.
3. Load a save and play. **Measure only in game.** Freezes on the title screen, during the first load and on
   loading screens are normal.
4. Stop it: quit the game, press Ctrl+C, or create an empty file `freezemon.stop` next to `freezemon.exe`.

Options:

```
freezemon [--fps 60] [--threshold 1.5] [--interval-us 125] [--top 40]
```

`--fps` is your frame rate cap, `--threshold` is the freeze threshold in frames, `--interval-us` is the CPU
sampling interval, and `--top` is how many hot locations to list per freeze.

## Output

The console prints one line per freeze. For example, the unmodified game during gameplay:

```
[freezemon] freeze #59 00:01:52:10 150.5 ms (8 missed), present thread busy 100% mostly game:.text#2
[freezemon] freeze #60 00:02:23:10 150.3 ms (8 missed), present thread busy 99% mostly game:.text#2
```

- `00:01:52:10`: time since the first frame freezemon saw (hours:minutes:seconds:frame).
- `150.5 ms (8 missed)`: length of the freeze and the number of frames lost.
- `present thread busy 100%`: the render thread was running on a CPU for the whole freeze, so the game was
  computing something rather than waiting.
- `mostly game:.text#2`: where most of that time went. `.text#2` is the copy protection's code section. A regular
  ~150 ms freeze spent there is the stutter that [freezefix](../freezefix/) removes.

`freezemon.log` (next to `freezemon.exe`) has the full breakdown of every freeze: per-thread busy share, zones,
the hottest code locations and the most frequent call stacks of the render thread. A shortened entry:

```
FREEZE #50  00:02:02:02  2026-11-23 01:09:20.721  149.7 ms  8 frame(s) missed
  present thread 18160: busy 95% (1137 samples)
    game:.text#2 84.3% | game:.text#1 gap 7.5% | game:.text#1 func 5.5% | ntdll.dll 1.2% | kernel 0.7% | other 0.7%
  all game threads: 1390 samples
    game:.text#2 69.0% | game:.text#1 func 10.7% | game:.text#1 gap 7.3% | nvwgf2umx.dll 4.6% | ntdll.dll 2.5% | other 5.8%
  top locations (all game threads):
       234   16.8%  game:.text#2           .text#2:0x5205889
       223   16.0%  game:.text#2           .text#2:0x5123b91
       180   12.9%  game:.text#2           .text#2:0x5123ba8
       137    9.9%  game:.text#2           .text#2:0x51228be
```

Zone names: `game:.text#1 func` is game code inside a known function, `game:.text#1 gap` is game code outside any
function table entry, `game:.text#2` is the protection's section, `name.dll` is another module (for example the GPU
driver), `kernel` is kernel time.

## Build

See [Verifying the binaries](../README.md#verifying-the-binaries-reproducible-build).
