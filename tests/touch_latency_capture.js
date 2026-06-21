/**
 * End-to-end touch input latency test.
 *
 * Drives a real WebRTC session exactly like webrtc_capture.js, then
 * dispatches a genuine `TouchEvent` on the <video> element -- the same DOM
 * path a finger press takes -- and measures, using the browser's own clock
 * via `video.requestVideoFrameCallback`, how long it takes for the decoded
 * frame to flip from black to white. The compositor-side test client
 * (`wayland-touch-client`) renders solid black while idle and solid white
 * while a touch point is down, so the flip is unambiguous even after H.264
 * lossy compression.
 */
import puppeteer from 'puppeteer';

const PORT = process.argv[2] || '8090';
const COMPOSITOR_URL = `http://localhost:${PORT}`;

const BRIGHTNESS_WHITE = 200;
const BRIGHTNESS_BLACK = 50;
const DETECT_TIMEOUT_MS = 8000;

async function run() {
    console.log('Launching browser...');
    const executablePath = '/usr/bin/chromium';
    const browser = await puppeteer.launch({
        headless: 'new',
        executablePath,
        args: [
            '--no-sandbox',
            '--disable-setuid-sandbox',
            '--disable-dev-shm-usage',
            '--use-fake-ui-for-media-stream',
            '--use-fake-device-for-media-stream',
        ],
    });

    try {
        const page = await browser.newPage();
        page.on('console', (msg) => console.log('[page]', msg.text()));

        console.log(`Navigating to ${COMPOSITOR_URL}...`);
        await page.goto(COMPOSITOR_URL, { waitUntil: 'networkidle2', timeout: 10000 });

        console.log('Waiting for video element...');
        await page.waitForSelector('video', { timeout: 10000 });
        await page.waitForFunction(
            () => document.querySelector('video')?.readyState >= 2,
            { timeout: 15000 }
        );

        // Let the stream settle on a stable frame before measuring -- the
        // first second or two can still contain the startup keyframe and
        // the client's initial viewport resize negotiation.
        console.log('Letting stream settle...');
        await new Promise((resolve) => setTimeout(resolve, 2000));

        console.log('Arming in-page frame sampler...');
        await page.evaluate((whiteThreshold, blackThreshold) => {
            const video = document.querySelector('video');
            const canvas = document.createElement('canvas');
            canvas.width = 8;
            canvas.height = 8;
            const ctx = canvas.getContext('2d', { willReadFrequently: true });

            window.__touchTest = {
                pressAt: null,
                whiteAt: null,
                releaseAt: null,
                blackAgainAt: null,
                sampleCount: 0,
            };

            function averageBrightness() {
                ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
                const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
                let sum = 0;
                let n = 0;
                for (let i = 0; i < data.length; i += 4) {
                    sum += (data[i] + data[i + 1] + data[i + 2]) / 3;
                    n++;
                }
                return sum / n;
            }

            function onFrame() {
                const t = performance.now();
                const state = window.__touchTest;
                state.sampleCount++;
                const brightness = averageBrightness();

                if (state.pressAt !== null && state.whiteAt === null && brightness >= whiteThreshold) {
                    state.whiteAt = t;
                }
                if (
                    state.releaseAt !== null &&
                    state.whiteAt !== null &&
                    state.blackAgainAt === null &&
                    brightness <= blackThreshold
                ) {
                    state.blackAgainAt = t;
                }

                video.requestVideoFrameCallback(onFrame);
            }
            video.requestVideoFrameCallback(onFrame);
        }, BRIGHTNESS_WHITE, BRIGHTNESS_BLACK);

        console.log('Dispatching synthetic touchstart on <video>...');
        await page.evaluate(() => {
            const video = document.querySelector('video');
            const rect = video.getBoundingClientRect();
            const x = rect.left + rect.width / 2;
            const y = rect.top + rect.height / 2;
            const touch = new Touch({ identifier: 1, target: video, clientX: x, clientY: y });
            const event = new TouchEvent('touchstart', {
                touches: [touch],
                targetTouches: [touch],
                changedTouches: [touch],
                bubbles: true,
                cancelable: true,
            });
            window.__touchTest.pressAt = performance.now();
            video.dispatchEvent(event);
        });

        console.log('Waiting for video to flip white...');
        await page.waitForFunction(() => window.__touchTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });

        const { pressAt, whiteAt } = await page.evaluate(() => window.__touchTest);
        const pressToWhiteMs = whiteAt - pressAt;
        console.log(`touch-down -> visual white flip: ${pressToWhiteMs.toFixed(1)} ms`);

        console.log('Dispatching synthetic touchend on <video>...');
        await page.evaluate(() => {
            const video = document.querySelector('video');
            const rect = video.getBoundingClientRect();
            const x = rect.left + rect.width / 2;
            const y = rect.top + rect.height / 2;
            const touch = new Touch({ identifier: 1, target: video, clientX: x, clientY: y });
            const event = new TouchEvent('touchend', {
                touches: [],
                targetTouches: [],
                changedTouches: [touch],
                bubbles: true,
                cancelable: true,
            });
            window.__touchTest.releaseAt = performance.now();
            video.dispatchEvent(event);
        });

        console.log('Waiting for video to flip back to black...');
        await page.waitForFunction(() => window.__touchTest.blackAgainAt !== null, { timeout: DETECT_TIMEOUT_MS });

        const { releaseAt, blackAgainAt, sampleCount } = await page.evaluate(() => window.__touchTest);
        const releaseToBlackMs = blackAgainAt - releaseAt;
        console.log(`touch-up   -> visual black flip: ${releaseToBlackMs.toFixed(1)} ms`);
        console.log(`(sampled ${sampleCount} decoded frames during the test)`);

        // Machine-readable line the Rust test harness parses.
        console.log(`RESULT pressToWhiteMs=${pressToWhiteMs.toFixed(1)} releaseToBlackMs=${releaseToBlackMs.toFixed(1)}`);

        await browser.close();
    } catch (error) {
        console.error('Touch latency capture failed:', error);
        await browser.close();
        process.exit(1);
    }
}

run().then(() => process.exit(0));
