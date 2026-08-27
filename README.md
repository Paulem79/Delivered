# Delivered
A simple file hosting, like, very simple

And so I remade it in rust because it was not that hard to port (I'm still not that good in rust, 01/05/2026), and it performs better !
## HLS streaming

Any `.mp4` dropped in `FILES_DIR/stream/` is also served as HLS, so a player can
seek and buffer instead of downloading the whole file. Segments are cut at
keyframes and remuxed to fragmented MP4 on the fly — no transcoding, no ffmpeg,
nothing written to disk.

| Endpoint | What it returns |
| --- | --- |
| `GET /api/stream` | JSON list of the available streams |
| `GET /api/stream/{name}/master.m3u8` | multivariant playlist |
| `GET /api/stream/{name}/index.m3u8` | media playlist |
| `GET /api/stream/{name}/init.mp4` | CMAF init segment |
| `GET /api/stream/{name}/segment-{n}.m4s` | CMAF media segment |

`{name}` is the file name without its extension, and `.m3u` works anywhere
`.m3u8` does. So `files/stream/demo.mp4` plays from:

```
http://localhost:3000/api/stream/demo/master.m3u8
```

The raw file stays available at `/stream/demo.mp4` for clients that would rather
download it.

Configuration lives in `.env.example`: `STREAM_DIR` picks the sub-directory and
`HLS_TARGET_DURATION` the nominal segment length in seconds.
