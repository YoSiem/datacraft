# Low-power rendering

DataCraft draws with DirectX 12 on Windows. On Windows Server without a GPU, and in
most Remote Desktop sessions, DirectX 12 runs on WARP (the "Microsoft Basic Render
Driver"), which rasterizes every frame on the CPU; RDP then encodes every frame again.
Both costs grow with the number of frames drawn.

At startup DataCraft reads the adapter wgpu selected. **Settings → Rendering** shows it,
marks a software adapter as "software (CPU)" and notes a Remote Desktop session
(`GetSystemMetrics(SM_REMOTESESSION)`).

| Mode | Behavior |
| --- | --- |
| Automatic (default) | Low-power on a software adapter or in a Remote Desktop session, full quality otherwise |
| Low-power | Always low-power |
| Full quality | Never low-power |

Low-power mode:

- applies each mouse wheel notch in one frame instead of animating it over ~10 frames;
- turns off widget animations and animated scrolling to a target;
- draws windows and popups without blurred shadows;
- caps the frame rate at 30 fps while the user interacts. Input received while waiting
  is handled by the next frame.

The application still repaints only on input or when a result arrives; idle CPU usage
is unchanged. MSAA and the depth buffer are already off in every mode.

**Show frame statistics** in Settings, or `DATACRAFT_FPS=1`, adds an overlay with the
frames drawn in the last second, the previous frame's time (painting included, frame-cap
wait excluded), the active mode and the adapter. Use it to compare modes on a server.
The mode and the Settings checkbox are remembered between sessions; `DATACRAFT_FPS=1`
applies to that run only.
