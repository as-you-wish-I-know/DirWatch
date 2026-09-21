# DirWatch Icon

Original artwork created July 2026 for the DirWatch application. This icon was designed
from scratch and does not derive from any copyrighted work — you are free to use, modify,
and distribute it without restriction.

## Files

- `dirwatch.ico` — multi-resolution Windows icon (16, 24, 32, 48, 64, 128, 256 px, 32-bit RGBA).
  This is the only file most applications need.
- `dirwatch.svg` — master vector source (detailed art, used for 48 px and up).
- `dirwatch_small.svg` — simplified vector source used for the 16/24/32 px renditions
  (bigger eye, thicker outlines, so it stays legible when tiny).
- `png/` — individual PNG renders at 16, 24, 32, 48, 64, 128, 256, and 512 px
  (512 px is handy for websites, installers, or documentation).

## Adding the icon to your application

### C# / .NET (WinForms or WPF or console)

Add to your `.csproj`:

```xml
<PropertyGroup>
  <ApplicationIcon>dirwatch.ico</ApplicationIcon>
</PropertyGroup>
```

This sets the icon embedded in the .exe (what Explorer and the taskbar show).

For WinForms, also set the window icon: `this.Icon = new Icon("dirwatch.ico");`
or assign it in the Form designer's `Icon` property.

For WPF, set it on the window: `<Window ... Icon="dirwatch.ico">` and/or in the
project properties as above.

### C / C++ (Win32)

Add to your resource script (`.rc` file):

```
IDI_APPICON ICON "dirwatch.ico"
```

Windows uses the lowest-numbered icon resource as the .exe's display icon.
To set the window icon at runtime:

```c
HICON hIcon = (HICON)LoadImage(hInstance, MAKEINTRESOURCE(IDI_APPICON),
                               IMAGE_ICON, 0, 0, LR_DEFAULTSIZE);
SendMessage(hwnd, WM_SETICON, ICON_BIG, (LPARAM)hIcon);
SendMessage(hwnd, WM_SETICON, ICON_SMALL, (LPARAM)hIcon);
```

### Other toolkits

- **Python (tkinter):** `root.iconbitmap("dirwatch.ico")`
- **Electron:** set `icon: "dirwatch.ico"` in the BrowserWindow options and packager config.
- **Qt:** `app.setWindowIcon(QIcon("dirwatch.ico"));`
- **Rust (winres):** in `build.rs`: `winres::WindowsResource::new().set_icon("dirwatch.ico")`

## Regenerating sizes

Any size can be re-rendered from the SVG sources, e.g. with cairosvg:

```
cairosvg dirwatch.svg -o out.png --output-width 300 --output-height 300
```

Use `dirwatch_small.svg` for renders at 32 px and below.
