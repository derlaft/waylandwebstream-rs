/**
 * End-to-end keyboard input latency test.
 *
 * Drives a real stream session exactly like mouse_latency_capture.js, then
 * uses Puppeteer's CDP-backed `page.keyboard` API to press/release a real
 * key -- trusted, browser-native input events, not JS-dispatched synthetic
 * ones, so this exercises the same path a real keyboard does. Measures,
 * using the browser's own clock via a `requestAnimationFrame` sampling
 * loop, how long it takes for the decoded frame to flip from black to
 * white. The compositor-side test client (`wayland-keyboard-client`)
 * renders solid black while idle and solid white while evdev `KEY_A` is
 * held, so the flip is unambiguous even after H.264 lossy compression.
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

        console.log('Waiting for canvas element...');
        await page.waitForSelector('canvas', { timeout: 10000 });
        await page.waitForFunction(
            () => {
                const canvas = document.querySelector('canvas');
                return canvas && canvas.width > 0 && canvas.height > 0;
            },
            { timeout: 15000 }
        );

        console.log('Letting stream settle...');
        await new Promise((resolve) => setTimeout(resolve, 2000));

        console.log('Arming in-page frame sampler...');
        await page.evaluate((whiteThreshold, blackThreshold) => {
            const canvas = document.querySelector('canvas');
            const sampleCanvas = document.createElement('canvas');
            sampleCanvas.width = 8;
            sampleCanvas.height = 8;
            const ctx = sampleCanvas.getContext('2d', { willReadFrequently: true });

            window.__keyTest = {
                downAt: null,
                whiteAt: null,
                upAt: null,
                blackAgainAt: null,
                sampleCount: 0,
            };

            function averageBrightness() {
                ctx.drawImage(canvas, 0, 0, sampleCanvas.width, sampleCanvas.height);
                const { data } = ctx.getImageData(0, 0, sampleCanvas.width, sampleCanvas.height);
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
                const state = window.__keyTest;
                state.sampleCount++;
                const brightness = averageBrightness();

                if (state.downAt !== null && state.whiteAt === null && brightness >= whiteThreshold) {
                    state.whiteAt = t;
                }
                if (
                    state.upAt !== null &&
                    state.whiteAt !== null &&
                    state.blackAgainAt === null &&
                    brightness <= blackThreshold
                ) {
                    state.blackAgainAt = t;
                }

                requestAnimationFrame(onFrame);
            }
            requestAnimationFrame(onFrame);
        }, BRIGHTNESS_WHITE, BRIGHTNESS_BLACK);

        const canvasRect = await page.evaluate(() => {
            const r = document.querySelector('canvas').getBoundingClientRect();
            return { left: r.left, top: r.top, width: r.width, height: r.height };
        });
        const centerX = canvasRect.left + canvasRect.width / 2;
        const centerY = canvasRect.top + canvasRect.height / 2;

        // Click the canvas first -- the client only forwards keystrokes once
        // it has focus (see Stage.svelte's `tabindex` + input.ts's
        // `canvas.focus()` on pointerdown).
        console.log('Clicking <canvas> to focus it...');
        await page.mouse.click(centerX, centerY);

        console.log('Pressing real OS-level "A" key...');
        await page.evaluate(() => { window.__keyTest.downAt = performance.now(); });
        await page.keyboard.down('a');

        console.log('Waiting for canvas to flip white...');
        await page.waitForFunction(() => window.__keyTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });

        const { downAt, whiteAt } = await page.evaluate(() => window.__keyTest);
        const downToWhiteMs = whiteAt - downAt;
        console.log(`keydown -> visual white flip: ${downToWhiteMs.toFixed(1)} ms`);

        console.log('Releasing "A" key...');
        await page.evaluate(() => { window.__keyTest.upAt = performance.now(); });
        await page.keyboard.up('a');

        console.log('Waiting for canvas to flip back to black...');
        await page.waitForFunction(() => window.__keyTest.blackAgainAt !== null, { timeout: DETECT_TIMEOUT_MS });

        const { upAt, blackAgainAt, sampleCount } = await page.evaluate(() => window.__keyTest);
        const upToBlackMs = blackAgainAt - upAt;
        console.log(`keyup   -> visual black flip: ${upToBlackMs.toFixed(1)} ms`);
        console.log(`(sampled ${sampleCount} decoded frames during the test)`);

        if (pageErrors.length > 0) {
            throw new Error(`Page reported errors during the test: ${pageErrors.join('; ')}`);
        }

        // Machine-readable line the Rust test harness parses.
        console.log(`RESULT downToWhiteMs=${downToWhiteMs.toFixed(1)} upToBlackMs=${upToBlackMs.toFixed(1)}`);

        await browser.close();
    } catch (error) {
        console.error('Keyboard latency capture failed:', error);
        await browser.close();
        process.exit(1);
    }
}

run().then(() => process.exit(0));
