class AudiobookPcmPlayer extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queue = [];
    this.offset = 0;
    this.port.onmessage = (event) => {
      if (event.data instanceof Float32Array) this.queue.push(event.data);
      if (event.data?.type === "clear") { this.queue = []; this.offset = 0; }
    };
  }

  process(_inputs, outputs) {
    const channels = outputs[0];
    if (!channels?.length) return true;
    const frames = channels[0].length;
    for (let frame = 0; frame < frames; frame += 1) {
      while (this.queue.length && this.offset >= this.queue[0].length) {
        this.queue.shift();
        this.offset = 0;
      }
      const sample = this.queue.length ? this.queue[0][this.offset++] : 0;
      for (const channel of channels) channel[frame] = sample;
    }
    return true;
  }
}

registerProcessor("audiobookai-pcm-player", AudiobookPcmPlayer);
