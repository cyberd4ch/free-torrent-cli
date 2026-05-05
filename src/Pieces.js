export default class Pieces {
    constructor(totalPieces) {
        this.requested = new Set();
        this.received = new Set();
        this.total = totalPieces;
    }

    // Returns true if this piece hasn't been requested or received yet
    needed(pieceIndex) {
        return !this.requested.has(pieceIndex) && !this.received.has(pieceIndex);
    }

    // Mark a piece as requested
    addRequested(pieceIndex) {
        this.requested.add(pieceIndex);
    }

    // Mark a piece as received (and remove from requested)
    addReceived(pieceIndex) {
        this.received.add(pieceIndex);
        this.requested.delete(pieceIndex);
    }

    // How many pieces remain
    get remaining() {
        return this.total - this.received.size;
    }

    // Fixed: Set has no .every(); compare sizes instead
    isDone() {
        return this.received.size === this.total;
    }
}