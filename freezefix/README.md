# freezefix

> **WARNING: use it only in Steam Offline Mode. Playing online with a modified game can get your account
> soft-banned. See the [main README](../README.md).**

Removes the periodic ~150 ms stutter of Dark Souls III 1.15.2.0.

The stutter comes from two integrity checks of the game's copy protection: hash loops over large memory regions
that run on the render thread. freezefix is a DLL that runs inside the game. Once the protection has finished
setting up the game, it applies three small patches in memory: it puts back the original first bytes of one hooked
function, so that its check no longer starts, and it turns the loop branches of two hash loops into NOPs. The
protection re-installs its hook from time to time, so a watchdog re-applies the patches within 250 ms.

What it does not do: change any file on disk, or disable the protection. All other protection hooks keep working,
and freezefix touches only its own three patch sites. On any game version other than 1.15.2.0 it applies no patch.

**Network block.** As soon as the DLL is loaded, it makes `WSAStartup` (Windows Sockets initialization) always fail,
so the game cannot open any network connection. This is built in and cannot be turned off; if the block cannot be
installed, freezefix closes the game instead of letting it run online. It applies on every game version. It is a
safety net against going online by accident, not a replacement for Steam Offline Mode: **still switch Steam to
Offline Mode before you play.**

## 1. Measure first (recommended)

Run [freezemon](../freezemon/) and play for 5 to 10 minutes **in game, with a save loaded**. Freezes on the title
screen, during the first load and on loading screens are normal and are not what this fixes. Without the fix
you will see regular entries like these, about every 30 seconds:

```
[freezemon] freeze #59 00:01:52:10 150.5 ms (8 missed), present thread busy 100% mostly game:.text#2
[freezemon] freeze #60 00:02:23:10 150.3 ms (8 missed), present thread busy 99% mostly game:.text#2
```

## 2. Install

1. Copy `bin\freezefix.dll` into the game folder, `...\steamapps\common\DARK SOULS III\Game\`, and rename it to
   **`dinput8.dll`**.
2. Switch Steam to Offline Mode and start the game from Steam as usual.

The game loads `dinput8.dll` from its own folder; freezefix passes every DirectInput call on to the Windows
`dinput8.dll`, so input works as usual. If another mod already uses `dinput8.dll` in the game folder, use
ModEngine2 instead (below).

<details>
<summary><b>Alternative: ModEngine2</b></summary>

Use this if you already play with ModEngine2, or if another mod occupies `dinput8.dll`.

1. Download ModEngine2 **2.1.0.0 or newer** from its
   [GitHub releases](https://github.com/soulsmods/ModEngine2/releases) and extract it to any folder.
2. Copy `bin\freezefix.dll` (keep its name) into that folder, next to `config_darksouls3.toml`.
3. In `config_darksouls3.toml`, add it to `external_dlls` in the `[modengine]` section, next to any entries you
   already have:
   ```toml
   [modengine]
   external_dlls = ["freezefix.dll"]
   ```
4. Make sure there is no freezefix `dinput8.dll` in the game folder, switch Steam to Offline Mode and start the
   game with `launchmod_darksouls3.bat`.

</details>

## 3. Check that it works

`freezefix.log` appears next to the DLL. A successful start looks like this:

```
[...] network blocked: ws2_32!WSAStartup @0x7ff... always fails (WSASYSNOTREADY)
[...] freezefix: base 0x7ff6..., version 1.15.2.0, 1 inplace + 2 raw patches
[...] all patch sites in place after ...
[...] patched inplace 0x8f55f0 [0x8f55f0,0x8f55fb) -> original bytes
[...] patched raw check2_hash_loop_collapse @0x7ff6... [48, 0f, 4d, d8] -> [90, 90, 90, 90]
[...] patched raw check2_hash_loop2_collapse @0x7ff6... [0f, 8f, ad, ff, ff, ff] -> [90, 90, 90, 90, 90, 90]
[...] network blocked: WSAStartup call refused (further calls are refused silently)
```

In the game itself, a message that the network is unavailable and the game will run offline is expected: that is
the network block at work.

Later you will usually see `REVERTED` followed by `re-applied ...; further reverts are handled silently`. That is
expected: the protection restored its hook and the watchdog put the patch back. It is logged only once, so the log
stays small.

Other messages:

- `version ... != 1.15.2.0; not patching`: unsupported game version; no stutter fix, but the network stays blocked.
- `patch sites never held their expected bytes ... not patching`: something else (another mod, or a different
  build) changed the same code; no stutter fix, the network stays blocked.
- `FAIL: ...; terminating the game`: the network block could not be installed, or a patch write could not be
  verified. freezefix closes the game rather than let it run online or half patched. Please report this.

## 4. Measure again

Run freezemon again in game. The regular ~150 ms entries should be gone. What remains is normal:

- short hitches of 1 or 2 frames (25 to 35 ms), which you will hardly notice while playing;
- one long freeze (about 0.5 s) while a save loads.

## Uninstall

Delete `dinput8.dll` (the renamed freezefix) from the game folder, or remove the `external_dlls` entry
(ModEngine2).

## Build

See [Verifying the binaries](../README.md#verifying-the-binaries-reproducible-build). `src/bodies.json` is the
patch table: offsets in `DarkSoulsIII.exe` 1.15.2.0 together with the expected and the replacement bytes. Every site
is checked against the expected bytes in the running game before anything is written.
