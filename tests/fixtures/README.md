These short audio fixtures are generated signals, with no third-party recordings.

`stereo24.flac` contains 441 stereo frames at 44.1 kHz and 24-bit precision. At frame `n`, the left sample is `((n % 17) - 8) / 8388608`, and the right sample is its negative. The decoder/output test compares every sample exactly.

`stereo.mp3` contains 0.1 seconds of 440 Hz left and 880 Hz right tones at 48 kHz, encoded at 128 kbps. It checks MP3 decoding, channel separation and source-rate preservation.

`stereo-gapless.mp3` uses the same tones for 0.9 seconds, with Xing/LAME encoder delay and padding metadata. The seek tests compare decoded samples against uninterrupted playback, including seeks that start partway through the file and require decoder preroll.

`continuous16.flac` and `continuous24.flac` split one 441-frame stereo waveform into 173 frames at 16-bit precision and 268 frames at 24-bit precision, both at 44.1 kHz. Across the combined files, frame `n` has left sample `((n % 17) - 8) / 32768` and its negative on the right. Handoff tests compare every output sample across several buffer sizes, including boundaries inside a buffer and exactly at its end.

Generated with FFmpeg:

```sh
ffmpeg -f lavfi -i 'aevalsrc=(mod(n\,17)-8)/8388608|-(mod(n\,17)-8)/8388608:s=44100:d=0.01' -c:a flac -sample_fmt s32 -bits_per_raw_sample 24 -fflags +bitexact -flags:a +bitexact -metadata_header_padding 0 stereo24.flac
ffmpeg -f lavfi -i 'aevalsrc=0.1*sin(2*PI*440*t)|0.1*sin(2*PI*880*t):s=48000:d=0.1' -c:a libmp3lame -b:a 128k -fflags +bitexact -flags:a +bitexact -write_xing 0 -map_metadata -1 stereo.mp3
ffmpeg -f lavfi -i 'aevalsrc=0.1*sin(2*PI*440*t)|0.1*sin(2*PI*880*t):s=48000:d=0.9' -c:a libmp3lame -b:a 128k -fflags +bitexact -flags:a +bitexact -map_metadata -1 stereo-gapless.mp3
ffmpeg -f lavfi -i 'aevalsrc=(mod(n\,17)-8)/32768|-(mod(n\,17)-8)/32768:s=44100:d=0.01' -af atrim=end_sample=173 -c:a flac -sample_fmt s16 -fflags +bitexact -flags:a +bitexact -metadata_header_padding 0 continuous16.flac
ffmpeg -f lavfi -i 'aevalsrc=(mod(n+173\,17)-8)/32768|-(mod(n+173\,17)-8)/32768:s=44100:d=0.01' -af atrim=end_sample=268 -c:a flac -sample_fmt s32 -bits_per_raw_sample 24 -fflags +bitexact -flags:a +bitexact -metadata_header_padding 0 continuous24.flac
```
