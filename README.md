# ale-server

HTTP API server for [Ale, My Eyes!](https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes) - intelligent visual assistant.

## Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/status` | GET | System status |
| `/models` | GET | Available models |
| `/asr/transcribe` | POST | Speech-to-text (multipart audio file) |
| `/tts/synthesize` | POST | Text-to-speech (JSON `{ "text": "..." }`) |
| `/vlm/describe` | POST | Image description (multipart image file) |
| `/vlm/ask` | POST | Vision Q&A (multipart: image + question field) |

## Usage

```bash
cargo run
```

Server starts on `0.0.0.0:8000`.

### Examples

```bash
# Health check
curl http://localhost:8000/health

# Transcribe audio
curl -F "file=@audio.wav" http://localhost:8000/asr/transcribe

# Describe image
curl -F "file=@photo.jpg" http://localhost:8000/vlm/describe

# Ask about image
curl -F "file=@screen.png" -F "question=What is shown?" http://localhost:8000/vlm/ask

# Text to speech
curl -X POST -H "Content-Type: application/json" -d '{"text":"Hello"}' http://localhost:8000/tts/synthesize
```

## Configuration

Uses the default `ale-core` configuration from `~/.config/ale-my-eyes/config.json`. Set your API key in the config file or through the GUI app first.

## License

MIT
