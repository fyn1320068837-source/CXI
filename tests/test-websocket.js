#!/usr/bin/env node
// WebSocket test client

const WebSocket = require('ws');

const ws = new WebSocket('ws://127.0.0.1:8080/ws');

ws.on('open', () => {
    console.log('✓ WebSocket connected');
});

ws.on('message', (data) => {
    console.log('Received:', data.toString());
});

ws.on('close', () => {
    console.log('WebSocket disconnected');
});

ws.on('error', (error) => {
    console.error('WebSocket error:', error.message);
});

// Keep alive for 10 seconds
setTimeout(() => {
    ws.close();
    process.exit(0);
}, 10000);
