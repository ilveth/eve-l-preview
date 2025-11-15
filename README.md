# eve-l-preview

An X11 EVE-O Preview lookalike. Works flawlessly on Wayland as long as you run Wine/Proton through XWayland (default behaviour).

## Features

- Highlight border for the active EVE client
- Left-click to focus a client
- Drag to reposition thumbnails
- Character name overlay
- Optional hide-when-unfocused mode
- Extremely lightweight (<1 MiB RAM)
- Fully configurable via environment variables

## Configuration

All configuration is done via environment variables:

| Variable | Type | Default | Description |
|-----------|------|----------|-------------|
| `WIDTH` | u16 | 240 | Thumbnail width |
| `HEIGHT` | u16 | 135 | Thumbnail height |
| `OPACITY` | u32 | `0xC0000000` | Thumbnail window opacity |
| `BORDER_SIZE` | u16 | 5 | Thumbnail border width |
| `BORDER_COLOR` | ARGB | `0x7FFF0000` | Border color |
| `TEXT_X` | i16 | 10 | Character name X coordinates  |
| `TEXT_Y` | i16 | 125 | Character name Y coordinates |
| `TEXT_FOREGROUND` | ARGB | `0xFFFFFFFF` | Text color |
| `TEXT_BACKGROUND` | ARGB | `0x7F000000` | Text background color |
| `HIDE_WHEN_NO_FOCUS` | bool | false | Hide thumbnails when all clients are unfocused |

> Colors and numeric values support both decimal and hex (`0x...`) input.

Example:

```bash
WIDTH=320 HEIGHT=180 BORDER_COLOR=0xFF00FF00 HIDE_WHEN_NO_FOCUS=true eve-l-preview
```

## Installation

### Binary

Prebuilt static binaries (linked against musl64) are available on the releases page.
No external runtime dependencies are required.

### Nix
Add the following to your `flake.nix`:
```nix
{
  inputs.eve-l-preview.url = "github:ilveth/eve-l-preview";
}
```
Then include it in your configuration:
```nix
environment.systemPackages = [ eve-l-preview.packages.${pkgs.system}.default ];
```

### Cargo

```bash
cargo install --locked --git https://github.com/ilveth/eve-l-preview
```

### Hyprland

For usage in Hyprland you need to set the following window rules:
```
windowrule = focusonactivate, title:^(EVE|EVE - .*)$
windowrule = pin, class:^eve-l-preview$
```

## Usage

Run it before or after launching EVE.
You can safely keep it running as it uses almost no resources especially under XWayland.
