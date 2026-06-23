/**
 * Stream capture script for automated testing.
 * Connects to the compositor's web interface (WebSocket + WebCodecs client)
 * and captures a decoded frame from the canvas.
 */

import puppeteer from 'puppeteer';

const COMPOSITOR_URL = 'http://localhost:8080';
const SCREENSHOT_PATH = process.argv[2] || '/tmp/compositor_test_screenshot.png';

async function captureFrame() {
    console.log('Launching browser...');

    // Use system chromium
    const executablePath = '/usr/bin/chromium';
    console.log(`Using Chromium at: ${executablePath}`);

    const browser = await puppeteer.launch({
        headless: 'new',
        executablePath: executablePath,
        args: [
            '--no-sandbox',
            '--disable-setuid-sandbox',
            '--disable-dev-shm-usage',
        ]
    });

    try {
        const page = await browser.newPage();

        console.log(`Navigating to ${COMPOSITOR_URL}...`);
        await page.goto(COMPOSITOR_URL, { waitUntil: 'networkidle2', timeout: 10000 });

        console.log('Waiting for canvas element...');
        await page.waitForSelector('canvas', { timeout: 10000 });

        // The client sizes the canvas to the decoded frame's dimensions on
        // the first frame it paints (see `handleFrame` in client.html), so
        // a non-zero size means at least one frame has been decoded.
        console.log('Waiting for first decoded frame...');
        await page.waitForFunction(
            () => {
                const canvas = document.querySelector('canvas');
                return canvas && canvas.width > 0 && canvas.height > 0;
            },
            { timeout: 15000 }
        );

        console.log('Frame decoded, waiting for a stable picture...');
        // Wait a bit more so we capture something past the first keyframe,
        // not just-decoded transient content.
        await new Promise((resolve) => setTimeout(resolve, 2000));

        console.log('Capturing screenshot...');
        const canvasElement = await page.$('canvas');
        await canvasElement.screenshot({ path: SCREENSHOT_PATH });

        console.log(`Screenshot saved to ${SCREENSHOT_PATH}`);

        const dimensions = await page.evaluate(() => {
            const canvas = document.querySelector('canvas');
            return { width: canvas.width, height: canvas.height };
        });

        console.log('Canvas dimensions:', dimensions);

        if (dimensions.width === 0 || dimensions.height === 0) {
            throw new Error('Canvas has invalid dimensions');
        }

        await browser.close();
        return true;

    } catch (error) {
        console.error('Error during capture:', error);
        await browser.close();
        process.exit(1);
    }
}

captureFrame().then(() => {
    console.log('Capture completed successfully');
    process.exit(0);
}).catch(error => {
    console.error('Fatal error:', error);
    process.exit(1);
});
