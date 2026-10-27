# Drop the gapless playback requirement

Gapless playback is out of scope. After listening to the cold-restart and prepared-stream experiments, the user chose to accept brief pauses between tracks rather than require seamless transitions. This supersedes the earlier gapless requirement, including for matching track formats. Native PipeWire output and best-effort sample preservation remain required. Playback should still avoid unintended clicks and clipping.
