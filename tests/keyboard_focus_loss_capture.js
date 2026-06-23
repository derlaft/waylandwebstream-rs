/**
 * Regression test for a stuck-modifier bug: a combo like Alt+Tab gets
 * intercepted by the OS/window manager and switches focus *away* from the
 * browser, so the keyup for Alt (or Tab) fires in whatever the OS switched
 * to -- never reaching the page at all. Without releasing held keys on
 * focus loss, the server considers that key held forever, silently
 * modifying every keystroke after it (see `releaseAllKeys` in
 * web/src/lib/input.ts).
 *
 * Reproduces this by pressing (CDP-level, real OS key event) evdev `KEY_A`
 * without ever sending a matching key-up, then switching the OS-level
 * focus away from the page (opening and foregrounding a second tab) --
 * exactly what happens to the browser window during a real Alt+Tab, minus
 * actually being a modifier combo. The compositor-side test client
 * (`wayland-keyboard-client`) must still see the key released (canvas
 * flips back to black) even though no `keyup` DOM event was ever dispatched
 * on the original page.
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

            window.__keyTest = { whiteAt: null, blackAgainAt: null };

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
                const state = window.__keyTest;
                const brightness = averageBrightness();
                if (state.whiteAt === null && brightness >= whiteThreshold) {
                    state.whiteAt = performance.now();
                }
                if (state.whiteAt !== null && state.blackAgainAt === null && brightness <= blackThreshold) {
                    state.blackAgainAt = performance.now();
                }
                requestAnimationFrame(onFrame);
            }
            requestAnimationFrame(onFrame);
        }, BRIGHTNESS_WHITE, BRIGHTNESS_BLACK);

        console.log('Confirming canvas is auto-focused without any click...');
        const activeTag = await page.evaluate(() => document.activeElement.tagName);
        if (activeTag !== 'CANVAS') {
            throw new Error(`Expected canvas to be auto-focused on load, but activeElement is <${activeTag.toLowerCase()}>`);
        }

        console.log('Pressing "A" with no matching key-up...');
        await page.keyboard.down('a');

        console.log('Waiting for canvas to flip white...');
        await page.waitForFunction(() => window.__keyTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });
        console.log('Confirmed: key reached the compositor.');

        console.log('Switching OS-level focus away from the page (like Alt+Tab) without releasing the key...');
        const decoyPage = await browser.newPage();
        await decoyPage.goto('about:blank');
        await decoyPage.bringToFront();
        await new Promise((resolve) => setTimeout(resolve, 500));

        // Backgrounding the page also pauses its own `requestAnimationFrame`
        // sampler (standard Chrome power-saving behavior), so the brightness
        // check below can't observe anything while still hidden -- that's a
        // limitation of this test's detection method, not of what's being
        // tested. The release-on-blur handler itself runs synchronously in
        // the `blur` event, independent of rAF, so it has already fired by
        // now; bringing the page back to front just resumes the ability to
        // *observe* that.
        await decoyPage.close();
        await page.bringToFront();

        console.log('Waiting for canvas to flip back to black (synthetic key-up on blur)...');
        await page.waitForFunction(() => window.__keyTest.blackAgainAt !== null, { timeout: DETECT_TIMEOUT_MS });
        console.log('Confirmed: losing focus released the stuck key.');

        // The browser never dispatched a real keyup for "a" (CDP still
        // considers it down), so explicitly release it before the next
        // check -- otherwise the *next* `page.keyboard.down('a')` would be
        // a no-op at the CDP level.
        await page.keyboard.up('a');

        console.log('Confirming the key can be pressed again after recovering...');
        await page.evaluate(() => { window.__keyTest.whiteAt = null; window.__keyTest.blackAgainAt = null; });
        await page.keyboard.down('a');
        await page.waitForFunction(() => window.__keyTest.whiteAt !== null, { timeout: DETECT_TIMEOUT_MS });
        await page.keyboard.up('a');
        await page.waitForFunction(() => window.__keyTest.blackAgainAt !== null, { timeout: DETECT_TIMEOUT_MS });
        console.log('Confirmed: normal press/release still works after recovery.');

        if (pageErrors.length > 0) {
            throw new Error(`Page reported errors during the test: ${pageErrors.join('; ')}`);
        }

        console.log('RESULT ok');
        await browser.close();
    } catch (error) {
        console.error('Keyboard focus-loss capture failed:', error);
        await browser.close();
        process.exit(1);
    }
}

run().then(() => process.exit(0));
