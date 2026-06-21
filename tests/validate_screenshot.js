/**
 * Screenshot validation script
 * Verifies that the captured frame is not blank and contains expected colors
 */

import fs from 'fs';
import { PNG } from 'pngjs';

const SCREENSHOT_PATH = process.argv[2] || '/tmp/compositor_test_screenshot.png';

async function validateScreenshot() {
    console.log(`Validating screenshot at ${SCREENSHOT_PATH}...`);
    
    if (!fs.existsSync(SCREENSHOT_PATH)) {
        console.error('Screenshot file does not exist!');
        process.exit(1);
    }
    
    const data = fs.readFileSync(SCREENSHOT_PATH);
    const png = PNG.sync.read(data);
    const pixels = png.data;
    const image = { width: png.width, height: png.height };
    
    console.log(`Image dimensions: ${image.width}x${image.height}`);
    
    // Calculate color distribution
    let redPixels = 0;
    let greenPixels = 0;
    let bluePixels = 0;
    let blackPixels = 0;
    let totalPixels = image.width * image.height;
    
    for (let i = 0; i < pixels.length; i += 4) {
        const r = pixels[i];
        const g = pixels[i + 1];
        const b = pixels[i + 2];
        const a = pixels[i + 3];
        
        // Skip transparent pixels
        if (a < 128) continue;
        
        // Check for predominantly red pixels (from our test client)
        if (r > 200 && g < 50 && b < 50) {
            redPixels++;
        }
        // Check for predominantly green pixels (from old test pattern)
        else if (g > 200 && r < 50 && b < 50) {
            greenPixels++;
        }
        // Check for predominantly blue pixels
        else if (b > 200 && r < 50 && g < 50) {
            bluePixels++;
        }
        // Check for black pixels
        else if (r < 20 && g < 20 && b < 20) {
            blackPixels++;
        }
    }
    
    const redPercent = (redPixels / totalPixels) * 100;
    const greenPercent = (greenPixels / totalPixels) * 100;
    const bluePercent = (bluePixels / totalPixels) * 100;
    const blackPercent = (blackPixels / totalPixels) * 100;
    
    console.log('Color distribution:');
    console.log(`  Red pixels: ${redPixels} (${redPercent.toFixed(2)}%)`);
    console.log(`  Green pixels: ${greenPixels} (${greenPercent.toFixed(2)}%)`);
    console.log(`  Blue pixels: ${bluePixels} (${bluePercent.toFixed(2)}%)`);
    console.log(`  Black pixels: ${blackPixels} (${blackPercent.toFixed(2)}%)`);
    
    // Validation criteria:
    // 1. Image should not be mostly black (indicates no rendering)
    if (blackPercent > 90) {
        console.error('FAIL: Image is mostly black - no rendering detected');
        process.exit(1);
    }
    
    // 2. Should have significant red content from our test client
    // OR significant green/blue from the gradient test pattern
    const hasContent = redPercent > 10 || greenPercent > 10 || bluePercent > 10;
    
    if (!hasContent) {
        console.error('FAIL: No significant colored content detected');
        process.exit(1);
    }
    
    console.log('PASS: Screenshot validation successful');
    console.log('The compositor is rendering content correctly!');
}

validateScreenshot().then(() => {
    process.exit(0);
}).catch(error => {
    console.error('Validation error:', error);
    process.exit(1);
});
