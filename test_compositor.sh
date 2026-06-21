#!/bin/bash
# Test script for WaylandWebStream compositor

echo "========================================"
echo "WaylandWebStream Compositor Test"
echo "========================================"
echo ""
echo "Starting compositor..."
./target/release/waylandwebstream &
COMP_PID=$!

sleep 3

echo ""
echo "Compositor is running!"
echo ""
echo "▶ Open your browser to: http://localhost:8080"
echo ""
echo "You should see:"
echo "  - Initially: Animated gradient test pattern (blue/green/red gradient)"
echo "  - After running a Wayland app: SOLID GREEN/TEAL background"
echo ""
echo "To test with weston-terminal, open another terminal and run:"
echo "  WAYLAND_DISPLAY=wayland-wws-0 weston-terminal"
echo ""
echo "The video should change from test pattern to solid green when"
echo "weston-terminal connects!"
echo ""
echo "Press Ctrl+C to stop the compositor"
echo ""

wait $COMP_PID
