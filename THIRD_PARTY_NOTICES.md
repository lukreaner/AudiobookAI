# Third-party notices

Release builds must replace this development notice with the generated dependency,
FFmpeg, LAME, eSpeak NG, WebView2, and frontend license inventory produced by the
release workflow. AudiobookAI itself is licensed under GPL-3.0-only.

## Optional app-managed Piper runtime

Piper 1.2.0 is not bundled with AudiobookAI. On Linux x86_64 it is downloaded
from the official `rhasspy/piper` v1.2.0 release only after an explicit user
install action, and its archive is SHA-256 verified before installation.

- Component: Piper 1.2.0
- Declared engine license: MIT
- Source: <https://github.com/rhasspy/piper/tree/v1.2.0>
- Release: <https://github.com/rhasspy/piper/releases/tag/v1.2.0>
- License text: <https://github.com/rhasspy/piper/blob/v1.2.0/LICENSE.md>

## Optional curated Piper voice

Voice artifacts are separate from the Piper engine and are not covered merely
by the engine's MIT license. AudiobookAI's initial catalog offers only
`de_DE-thorsten-medium`, pinned to `rhasspy/piper-voices` commit
`f5a6e9094787fd865d65cb024472f977f9c542b5`. Its ONNX model, JSON config, and
model card are each independently checksum-locked. The pinned model card
declares the source dataset license as CC0; that is a scoped model-card dataset
declaration, not an unqualified license assertion for unrelated repository
artifacts. AudiobookAI displays this declaration and the voice provenance and
requires confirmation before download.

- Voice: `de_DE-thorsten-medium`
- Pinned model card: <https://huggingface.co/rhasspy/piper-voices/blob/f5a6e9094787fd865d65cb024472f977f9c542b5/de/de_DE/thorsten/medium/MODEL_CARD>
- Declared source-dataset license: CC0-1.0
- License information: <https://creativecommons.org/publicdomain/zero/1.0/>
- Upstream voice source: <https://github.com/thorstenMueller/deep-learning-german-tts>
