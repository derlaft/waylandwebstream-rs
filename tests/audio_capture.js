/**
 * End-to-end audio test: verifies the /audio WebSocket endpoint is reachable,
 * stays open (audio capture is running), and delivers correctly framed Opus
 * packets to a browser.
 *
 * Wire format per message: [u64 pts_us BE (8 bytes)][raw Opus packet]
 *
 * Run against a live server:
 *   node tests/audio_capture.js [port]
 *
 * Exit 0 on success, 1 on failure.
 */

import puppeteer from 'puppeteer';

const PORT = process.argv[2] || '8080';
const URL = `http://localhost:${PORT}`;

// How long to wait for the first audio packet after the page loads.
const AUDIO_PACKET_TIMEOUT_MS = 15_000;
// Minimum number of audio packets to receive before declaring success.
const MIN_PACKETS = 3;

async function testAudio() {
  console.log(`Launching browser, connecting to ${URL} …`);

  const browser = await puppeteer.launch({
    headless: 'new',
    executablePath: '/usr/bin/chromium',
    args: [
      '--no-sandbox',
      '--disable-setuid-sandbox',
      '--disable-dev-shm-usage',
      // Allow audio playback without a user gesture so AudioContext starts
      // running in headless mode.
      '--autoplay-policy=no-user-gesture-required',
    ],
  });

  try {
    const page = await browser.newPage();

    // Capture console output from the page for debugging.
    page.on('console', (msg) => console.log(`[browser] ${msg.type()}: ${msg.text()}`));
    page.on('pageerror', (err) => console.error(`[browser] uncaught: ${err.message}`));

    // Inject a global counter that our raw /audio WebSocket will populate.
    // We open a second, independent WebSocket (not the one AudioStream opens)
    // to inspect the wire format without depending on WebCodecs/AudioContext.
    await page.evaluateOnNewDocument(() => {
      window.__audioTest = {
        packets: [],
        wsState: 'connecting',
        error: null,
      };

      const wsProto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const ws = new WebSocket(`${wsProto}//${location.host}/audio`);
      ws.binaryType = 'arraybuffer';

      ws.onopen = () => { window.__audioTest.wsState = 'open'; };
      ws.onerror = (e) => { window.__audioTest.error = String(e); };
      ws.onclose = (e) => {
        window.__audioTest.wsState = 'closed';
        window.__audioTest.closeReason = e.reason;
        window.__audioTest.closeCode = e.code;
      };
      ws.onmessage = (ev) => {
        const buf = ev.data;
        if (!(buf instanceof ArrayBuffer) || buf.byteLength < 8) {
          window.__audioTest.error = `bad frame: byteLength=${buf.byteLength}`;
          return;
        }
        const view = new DataView(buf);
        const hi = view.getUint32(0, false);
        const lo = view.getUint32(4, false);
        const pts_us = hi * 2 ** 32 + lo;
        window.__audioTest.packets.push({
          pts_us,
          opusBytes: buf.byteLength - 8,
        });
      };
    });

    console.log(`Navigating to ${URL} …`);
    await page.goto(URL, { waitUntil: 'networkidle2', timeout: 15_000 });

    // Simulate a user gesture so AudioContext.resume() succeeds and the page's
    // own AudioStream starts playing.
    await page.click('canvas').catch(() => { /* canvas might not be ready yet */ });

    // Wait for the WS to open.
    await page.waitForFunction(
      () => window.__audioTest.wsState !== 'connecting',
      { timeout: 5_000 },
    ).catch(() => {
      throw new Error('Timed out waiting for /audio WebSocket to connect');
    });

    const wsState = await page.evaluate(() => window.__audioTest.wsState);
    if (wsState === 'closed') {
      const { closeCode, closeReason } = await page.evaluate(() => ({
        closeCode: window.__audioTest.closeCode,
        closeReason: window.__audioTest.closeReason,
      }));
      if (closeReason === 'audio capture not available') {
        // PipeWire capture failed to start on this server — skip rather than
        // fail so the test doesn't block on machines without PipeWire.
        console.warn(`SKIP: server reports audio capture not available (code=${closeCode})`);
        return { skipped: true };
      }
      throw new Error(`/audio WebSocket closed unexpectedly: code=${closeCode} reason="${closeReason}"`);
    }

    console.log('/audio WebSocket is open — waiting for audio packets …');

    // Wait until we have received MIN_PACKETS packets or time out.
    await page.waitForFunction(
      (minPackets) => window.__audioTest.packets.length >= minPackets,
      { timeout: AUDIO_PACKET_TIMEOUT_MS },
      MIN_PACKETS,
    ).catch(() => {
      throw new Error(
        `Timed out: expected ≥${MIN_PACKETS} audio packets within ${AUDIO_PACKET_TIMEOUT_MS}ms`
      );
    });

    const result = await page.evaluate(() => ({
      packets: window.__audioTest.packets,
      error: window.__audioTest.error,
    }));

    if (result.error) {
      throw new Error(`Audio wire-format error: ${result.error}`);
    }

    const pkts = result.packets;
    console.log(`Received ${pkts.length} audio packets.`);

    // Verify each packet has a valid structure.
    for (let i = 0; i < pkts.length; i++) {
      const { pts_us, opusBytes } = pkts[i];
      if (opusBytes <= 0) throw new Error(`Packet ${i}: empty Opus payload`);
      if (pts_us < 0) throw new Error(`Packet ${i}: negative PTS (${pts_us})`);
    }

    // PTS should be monotonically non-decreasing.
    for (let i = 1; i < pkts.length; i++) {
      if (pkts[i].pts_us < pkts[i - 1].pts_us) {
        throw new Error(
          `Non-monotonic PTS: packet ${i} PTS=${pkts[i].pts_us} < packet ${i-1} PTS=${pkts[i-1].pts_us}`
        );
      }
    }

    // Consecutive PTS should be ~20 000 µs apart (one 20 ms Opus frame).
    if (pkts.length >= 2) {
      const gap = pkts[pkts.length - 1].pts_us - pkts[0].pts_us;
      const expectedGap = (pkts.length - 1) * 20_000;
      const tolerance = expectedGap * 0.25; // 25 % — generous for jitter
      if (Math.abs(gap - expectedGap) > tolerance) {
        throw new Error(
          `PTS gap ${gap}µs is far from expected ${expectedGap}µs (${pkts.length - 1} × 20000)`
        );
      }
      console.log(`PTS stride OK: ${gap}µs over ${pkts.length - 1} frames (expected ${expectedGap}µs)`);
    }

    return { skipped: false };
  } finally {
    await browser.close();
  }
}

testAudio().then(({ skipped }) => {
  if (skipped) {
    console.log('Test skipped (audio capture unavailable on server).');
    process.exit(0);
  }
  console.log('Audio e2e test PASSED.');
  process.exit(0);
}).catch((err) => {
  console.error('Audio e2e test FAILED:', err.message);
  process.exit(1);
});
