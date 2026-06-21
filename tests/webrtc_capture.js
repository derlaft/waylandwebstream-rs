/**
 * WebRTC capture script for automated testing
 * Connects to the compositor's web interface and captures a frame
 */

import puppeteer from 'puppeteer';
import fs from 'fs';

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
            '--use-fake-ui-for-media-stream',
            '--use-fake-device-for-media-stream',
        ]
    });

    try {
        const page = await browser.newPage();
        
        console.log(`Navigating to ${COMPOSITOR_URL}...`);
        await page.goto(COMPOSITOR_URL, { waitUntil: 'networkidle2', timeout: 10000 });
        
        console.log('Waiting for video element...');
        await page.waitForSelector('video', { timeout: 10000 });
        
        // Wait for video to start playing
        await page.waitForFunction(
            () => {
                const video = document.querySelector('video');
                return video && video.readyState >= 2; // HAVE_CURRENT_DATA
            },
            { timeout: 15000 }
        );
        
        console.log('Video is playing, waiting for stable frame...');
        // Wait a bit more to ensure we get a proper frame (not just black)
        await page.waitForTimeout(2000);
        
        console.log('Capturing screenshot...');
        const videoElement = await page.$('video');
        await videoElement.screenshot({ path: SCREENSHOT_PATH });
        
        console.log(`Screenshot saved to ${SCREENSHOT_PATH}`);
        
        // Get video dimensions for validation
        const dimensions = await page.evaluate(() => {
            const video = document.querySelector('video');
            return {
                width: video.videoWidth,
                height: video.videoHeight,
                readyState: video.readyState,
            };
        });
        
        console.log('Video dimensions:', dimensions);
        
        if (dimensions.width === 0 || dimensions.height === 0) {
            throw new Error('Video has invalid dimensions');
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
