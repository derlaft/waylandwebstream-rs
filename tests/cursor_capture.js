/**
 * End-to-end cursor rendering test.
 *
 * Connects to a running compositor (whose URL is passed as the first CLI arg),
 * waits for the video canvas to appear, then moves the Puppeteer mouse over
 * the canvas.  The wayland-cursor-client companion process responds to the
 * resulting wl_pointer.enter by calling wl_pointer.set_cursor with a solid-
 * magenta 16×16 surface.  The compositor reads those pixels and pushes a
 * ServerMessage::Cursor message to connected /ws clients; Stage.svelte applies
 * it as a CSS data-URL cursor on the <canvas>.  The test verifies that the
 * canvas cursor style contains `url(` (i.e. a custom data-URL cursor) within
 * a generous timeout.
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

        // Arm a watcher that checks whether the cursor style has changed to a
        // data-URL (i.e. a custom surface cursor).  We poll via
        // requestAnimationFrame rather than MutationObserver because the style
        // attribute is set imperatively by Svelte, not via classList.
        await page.evaluate(() => {
            window.__cursorChanged = false;
            function check() {
                const cur = document.querySelector('canvas')?.style.cursor ?? '';
                if (cur.includes('url(')) {
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

        const finalCursor = await page.evaluate(
            () => document.querySelector('canvas').style.cursor,
        );
        console.log(`Canvas cursor after pointer enter: "${finalCursor.slice(0, 80)}..."`);

        if (!finalCursor.includes('url(')) {
            throw new Error(`Expected cursor to contain url(, got: ${finalCursor}`);
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
