# Thio's SVG Thumbnail Extension for Windows Explorer

A high-performance thumbnail provider for Windows that generates explorer thumbnails for `.svg` and `.svgz` files, written in Rust, with no third party dependencies.

## Why Use This One?

<table>
  <tbody>
    <tr>
      <td><b>No Third-Party Dependencies</b></td>
      <td>
        Built using only official Microsoft-published Rust crates (found in the <a href="https://github.com/microsoft/windows-rs"><code>windows&#8209;rs</code></a> repo)
        <ul>
          ↳ No unknown code pulled in from an endless tree of dependencies
        </ul>
      </td>
    </tr>
    <tr>
      <td><b>Renders Purely Via the Windows API</b></td>
      <td>
        No separate third party library files that may go out of date or add overhead
      </td>
    </tr>
    <tr>
      <td><b>Trusted Certificate Signed</b></td>
      <td>
        Signed via Azure Trusted Signing, which requires rigorous verification of real-world identity.
        <ul>
          ↳ Proves it's not made by an unaccountable/unknown developer
        </ul>
      </td>
    </tr>
  </tbody>
</table>

Note: Also see [current limitations](#current-limitations) section

## Screenshot
<p align="center">
<img width=650 src=https://github.com/user-attachments/assets/67050436-809e-437b-9c17-4cdeeb386450>
</p>

# How To Install

> [!NOTE]
> After installing, be sure to disable any other SVG thumbnail extensions which may interfere, like the one from PowerToys.

### Option 1 (Easiest): Install With WinGet

Open command prompt and run this command, which will automatically download and run the installer.
```
winget install ThioJoe.SvgThumbnailExtension
```


### Option 2: Download the Installer
1.  Go to the [Releases](https://github.com/ThioJoe/win-svg-thumbs-rust/releases) page.
2.  For the latest release, look under `Assets` and download the `.msi` installer and run it.
3.  Windows Explorer will now automatically use this provider to display thumbnails for `.svg` and `.svgz` files.


#### How To Uninstall:
 - It can be uninstalled like any other app in Windows' Installed Apps list

<p align="center">
 <img width="386" height="204" alt="image" src="https://github.com/user-attachments/assets/2a2cc628-7d5f-4077-925a-37b59f0a4725" />
</p>

------

## Current Limitations:
- Currently, a small fraction of SVGs may render as black squares or as being filled completely black
  - Such SVGs contain properties not [supported by the Direct2D API](https://learn.microsoft.com/en-us/windows/win32/direct2d/svg-support) which this extension uses
  - There is also no support for text glyphs
- Overall, a vast majority of SVGs should render correctly. If you notice any from a particular program that consistently don't render, you can create an issue and I can see if anything can be done.


## How it works (Technical Details)

When Windows Explorer needs a thumbnail for a `.svg` file, it interacts with this DLL through a series of steps:

1.  **Initialization**: Explorer provides the `.svg` file's data as a stream to the DLL. The provider reads this entire stream into memory.
2.  **SVG Data Pre-Processing**:
    * The API doesn't support CSS `<style>` blocks within a separate dedicated <def> block, or at the top level. Therefore the script does some pre-processing on the XML so such SVGs will look correct
    * It uses the MSXML Windows API to look for `<style>` tags that apply styles to classes or named attributes
    * Then because the Direct2D API *does* support in-line style strings, the code does some rudimentary CSS parsing and applies the styles to each individual element (also using MSXML) before passing it to the Direct2D API.
4.  **Direct2D Rendering**:
    * The provider uses the **Direct2D** graphics API for high-performance, hardware-accelerated rendering.
    * First it creates a GPU-based bitmap to serve as a render target.
    * The Direct2D API turns the SVG data into a `SvgDocument` object. The `viewport` attribute is set to the thumbnail size requested by Explorer.
      * Fun fact: Though undocumented, the Direct2D API also will accept `.svgz` data, which is simply gzip-compressed svg data
    * The `width` and `height` attributes are removed from the root `<svg>` element before drawing, which I discovered causes the `DrawSvgDocument` method to autoscale the image to the viewport, avoiding the need to do any manual scaling to fill the thumbnail.
    * The `SvgDocument` is then drawn onto the render target bitmap.
    * The `unpremultiply` effect is then applied to the bitmap, because the standard Windows GDI requires straight alpha.
      * The un-premultiplication step is necessary for displaying transparency correctly and prevents dark edges from appearing on the final thumbnail.
5.  **Pixel Data Transfer**:
    * The rendered image is copied from the GPU render target to a "staging" bitmap on the CPU, which allows the program to access the raw pixel data.
    * This data is in a 32-bit BGRA format with **straight alpha** (after the unpremultiply effect).
    * Note: Although the staging bitmap is declared to Direct2D as having premultiplied alpha format (required for proper bitmap operations), the actual pixel values contain straight alpha data due to the unpremultiply effect. Direct2D normally only works with premultiplied alpha, but this doesn't matter at this point since we're just copying the data out to GDI.
6.  **Creating the Final Thumbnail**:
    * A standard Windows GDI `HBITMAP` is created, which is the final format Explorer needs for the thumbnail.
    * The pixel data is copied from the staging bitmap to the final `HBITMAP`.
7.  **Safety**: The entire thumbnail generation process is wrapped in a panic handler (`catch_unwind`). This ensures that if any unexpected error occurs during rendering, it will not crash the host application (e.g., `explorer.exe`).

## How to Manually Register DLL Yourself (Advanced)
If you want to manually register the DLL yourself instead of using the MSI installer, follow these steps:
1.  Go to the [Releases](https://github.com/ThioJoe/win-svg-thumbs-rust/releases) page.
2.  Download the latest `win_svg_thumbs.dll` file.
     - **IMPORTANT:** For security, place it somewhere that requires admin access to write, such as making a folder in `C:\Program Files`
4.  Open a Command Prompt with **administrator privileges**.
     - (Administrator is required or you will get error `0x80004005` for lack of permission)
5.  Navigate to the directory where you saved the `.dll` file.
6.  Run the following command to register the DLL:
    ```
    regsvr32 win_svg_thumbs.dll
    ```

To uninstall, run the following command in an administrator Command Prompt:
  ```
  regsvr32 /u win_svg_thumbs.dll
  ```

## How to Compile it Yourself

### Prerequisites

* Setup Rust on Windows: [See these instructions](https://learn.microsoft.com/en-us/windows/dev-environment/rust/setup)

### Instructions

1.  Clone the repository:
    ```
    git clone https://github.com/ThioJoe/win-svg-thumbs-rust
    ```
2.  Navigate to the project directory:
    ```
    cd win-svg-thumbs-rust
    ```
3.  Build the project in release mode:
    ```
    cargo build --release
    ```
4.  The compiled DLL will be located in the `target/release` directory.
