/**
 * End-to-end mouse input latency test.
 *
 * Drives a real WebRTC session exactly like webrtc_capture.js, then uses
 * Puppeteer's CDP-backed `page.mouse` API to move the OS-level mouse and
 * press/release a real button over the <video> element -- trusted,
 * browser-native input events, not JS-dispatched synthetic ones, so this
 * exercises the same path a real desktop mouse does. Measures, using the
 * browser's own clock via `video.requestVideoFrameCallback`, how long it
 * takes for the decoded frame to flip from black to white. The
 * compositor-side test client (`wayland-pointer-client`) renders solid black
 * while idle and solid white while a pointer button is held, so the flip is
 * unambiguous even after H.264 lossy compression.
 */
import puppeteer from 'puppeteer';

const PORT = process.argv[2] || '8091';
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
        const pageErrors = [];
        page.on('console', (msg) => console.log('[page]', msg.text()));
        page.on('pageerror', (err) => {
            console.error('[pageerror]', err.message);
            pageErrors.push(err.message);
        });

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

            window.__mouseTest = {
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
                const state = window.__mouseTest;
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

        const videoRect = await page.evaluate(() => {
            const r = document.querySelector('video').getBoundingClientRect();
            return { left: r.left, top: r.top, width: r.width, height: r.height };
        });
        const centerX = videoRect.left + videoRect.width / 2;
        const centerY = videoRect.top + videoRect.height / 2;

        console.log('Moving real OS-level mouse onto <video>...');
        await page.mouse.move(centerX, centerY);

        console.log('Pressing real OS-level mouse button...');
        await page.evaluate(() => { window.__mouseTest.pressAt = performance.now(); });
        await page.mouse.down();

        console.log('Waiting for video to flip white...');
        await page.waitForFunction(() => window.__mouseTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });

        const { pressAt, whiteAt } = await page.evaluate(() => window.__mouseTest);
        const pressToWhiteMs = whiteAt - pressAt;
        console.log(`mouse down -> visual white flip: ${pressToWhiteMs.toFixed(1)} ms`);

        console.log('Releasing real OS-level mouse button...');
        await page.evaluate(() => { window.__mouseTest.releaseAt = performance.now(); });
        await page.mouse.up();

        console.log('Waiting for video to flip back to black...');
        await page.waitForFunction(() => window.__mouseTest.blackAgainAt !== null, { timeout: DETECT_TIMEOUT_MS });

        const { releaseAt, blackAgainAt, sampleCount } = await page.evaluate(() => window.__mouseTest);
        const releaseToBlackMs = blackAgainAt - releaseAt;
        console.log(`pointerup   -> visual black flip: ${releaseToBlackMs.toFixed(1)} ms`);
        console.log(`(sampled ${sampleCount} decoded frames during the test)`);

        // Drag: press, move through several intermediate points while held,
        // then release. Exercises continuous pointermove-while-captured,
        // which a single click does not.
        console.log('Dragging across <video> with the button held...');
        await page.evaluate(() => { window.__mouseTest.whiteAt = null; });
        await page.mouse.move(centerX - 100, centerY - 50);
        await page.mouse.down();
        for (const [dx, dy] of [[20, 0], [40, 20], [60, -10], [80, 30]]) {
            await page.mouse.move(centerX - 100 + dx, centerY - 50 + dy);
        }
        await page.waitForFunction(() => window.__mouseTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });
        await page.mouse.up();
        console.log('Drag completed without error.');

        // Right click: a second physical button must reach the compositor
        // too, not just the primary (left) one.
        console.log('Right-clicking on <video>...');
        await page.evaluate(() => { window.__mouseTest.whiteAt = null; window.__mouseTest.blackAgainAt = null; });
        await page.mouse.move(centerX, centerY);
        await page.mouse.down({ button: 'right' });
        await page.waitForFunction(() => window.__mouseTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });
        await page.mouse.up({ button: 'right' });
        await page.waitForFunction(() => window.__mouseTest.blackAgainAt !== null, { timeout: DETECT_TIMEOUT_MS });
        console.log('Right click flipped white then black as expected.');

        // Wheel: must not throw or wedge the pointer pipeline for whatever
        // comes after it.
        console.log('Scrolling the wheel over <video>...');
        await page.mouse.wheel({ deltaY: 120 });
        console.log('Wheel scroll completed without error.');

        if (pageErrors.length > 0) {
            throw new Error(`Page reported errors during the test: ${pageErrors.join('; ')}`);
        }

        // Machine-readable line the Rust test harness parses.
        console.log(`RESULT pressToWhiteMs=${pressToWhiteMs.toFixed(1)} releaseToBlackMs=${releaseToBlackMs.toFixed(1)}`);

        await browser.close();
    } catch (error) {
        console.error('Mouse latency capture failed:', error);
        await browser.close();
        process.exit(1);
    }
}

run().then(() => process.exit(0));
