#!/usr/bin/env node
import { open } from './src/TorrentParser.js';
import downloadTorrent from './src/download.js';
import chalk from 'chalk';
import path from 'path';

// Catch anything that slips past try/catch so the process never exits silently
process.on('uncaughtException', err => {
    console.error(chalk.red('\n  Fatal error:'), err.message);
    console.error(err.stack);
    process.exit(1);
});

process.on('unhandledRejection', err => {
    console.error(chalk.red('\n  Unhandled rejection:'), err?.message ?? err);
    console.error(err?.stack ?? '');
    process.exit(1);
});

const args = process.argv.slice(2);

if (args.length === 0) {
    console.log('');
    console.log(chalk.bold.cyan('  free-torrent-cli'));
    console.log('');
    console.log(chalk.white('  Usage:'));
    console.log(chalk.gray('    node index.js <torrent-file> [output-dir]'));
    console.log('');
    console.log(chalk.white('  Examples:'));
    console.log(chalk.gray('    node index.js kali.torrent'));
    console.log(chalk.gray('    node index.js kali.torrent ~/Downloads'));
    console.log('');
    process.exit(0);
}

const torrentFile = args[0];
const outputDir = args[1] ? path.resolve(args[1]) : process.cwd();

try {
    const torrent = await open(torrentFile);
    await downloadTorrent(torrent, outputDir);
} catch (error) {
    console.error(chalk.red('\n  Error:'), error.message);
    console.error(error.stack);
    process.exit(1);
}