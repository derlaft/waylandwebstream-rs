/**
 * End-to-end cursor rendering test.
 *
 * Connects to a running compositor (whose URL is passed as the first CLI arg),
 * waits for the video canvas to appear, then moves the Puppeteer mouse over
 * the canvas.  The wayland-cursor-client companion process responds to the
 * resulting wl_pointer.enter by calling wl_pointer.set_cursor with a solid-
 * magenta 16×16 surface.  The compositor reads those pixels and pushes a
 * ServerMessage::Cursor message to connected /ws clients; Stage.svelte decodes
 * the RGBA pixels and renders them via a positioned <img> overlay (cursor:none
 * is always set on the canvas and stage so the OS cursor stays hidden).  The
 * test verifies that the overlay img src is a PNG data URL and is visible.
 *
 * Prints:   RESULT cursor_set=true   on success.
 * Exits 1 on any failure.
 */
import puppeteer from 'puppeteer';

const PORT = process.argv[2] || '8091';
const COMPOSITOR_URL = `http://localhost:${PORT}`;
const CURSOR_TIMEOUT_MS = 8000;

async function run() {
    console.log('Launching browser...');
    const browser = await puppeteer.launch({
        headless: 'new',
        executablePath: '/usr/bin/chromium',
        args: [
            '--no-sandbox',
            '--disable-setuid-sandbox',
            '--disable-dev-shm-usage',
        ],
    });

    try {
        const page = await browser.newPage();
        page.on('console', (msg) => console.log('[page]', msg.text()));
        page.on('pageerror', (err) => console.error('[pageerror]', err.message));

        console.log(`Navigating to ${COMPOSITOR_URL}...`);
        await page.goto(COMPOSITOR_URL, { waitUntil: 'networkidle2', timeout: 10000 });

        console.log('Waiting for canvas element with content...');
        await page.waitForSelector('canvas', { timeout: 10000 });
        await page.waitForFunction(
            () => {
                const canvas = document.querySelector('canvas');
                return canvas && canvas.width > 0 && canvas.height > 0;
            },
            { timeout: 15000 },
        );

        // Let the stream and control channel settle before moving the mouse.
        console.log('Letting stream settle (2s)...');
        await new Promise((r) => setTimeout(r, 2000));

        // Record the cursor style *before* any mouse interaction so we can
        // tell it apart from the post-enter style.
        const initialCursor = await page.evaluate(
            () => document.querySelector('canvas').style.cursor,
        );
        console.log(`Initial canvas cursor: "${initialCursor}"`);

        // Arm a watcher that checks whether the cursor overlay <img> has been
        // populated with a PNG data URL (i.e. a surface cursor was received).
        // The overlay approach keeps canvas.style.cursor as "none" always;
        // the cursor image lives in the src of .cursor-overlay instead.
        await page.evaluate(() => {
            window.__cursorChanged = false;
            function check() {
                const img = document.querySelector('.cursor-overlay');
                if (img && img.src.includes('data:image/') && img.style.display !== 'none') {
                    window.__cursorChanged = true;
                } else {
                    requestAnimationFrame(check);
                }
            }
            requestAnimationFrame(check);
        });

        // Move the Puppeteer (OS-level) mouse to the centre of the canvas so
        // the compositor delivers wl_pointer.enter to the cursor client.
        const rect = await page.evaluate(() => {
            const r = document.querySelector('canvas').getBoundingClientRect();
            return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
        });
        console.log(`Moving mouse to canvas centre (${rect.x.toFixed(0)}, ${rect.y.toFixed(0)})...`);
        await page.mouse.move(rect.x, rect.y);

        // Wait for the canvas cursor to become a data-URL.
        console.log(`Waiting up to ${CURSOR_TIMEOUT_MS}ms for cursor update...`);
        await page.waitForFunction(() => window.__cursorChanged, {
            timeout: CURSOR_TIMEOUT_MS,
        });

        const overlayInfo = await page.evaluate(() => {
            const img = document.querySelector('.cursor-overlay');
            return img
                ? { src: img.src.slice(0, 80), display: img.style.display }
                : null;
        });
        console.log(`Cursor overlay after pointer enter: ${JSON.stringify(overlayInfo)}`);

        if (!overlayInfo || !overlayInfo.src.includes('data:image/') || overlayInfo.display === 'none') {
            throw new Error(`Expected cursor overlay to be visible with a PNG src, got: ${JSON.stringify(overlayInfo)}`);
        }

        console.log('RESULT cursor_set=true');
        await browser.close();
    } catch (err) {
        console.error('Cursor capture failed:', err);
        await browser.close();
        process.exit(1);
    }
}

run().then(() => process.exit(0));
