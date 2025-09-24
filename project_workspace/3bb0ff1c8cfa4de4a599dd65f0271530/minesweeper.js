class MinesweeperGame {
    constructor() {
        this.board = [];
        this.revealed = [];
        this.flagged = [];
        this.mines = [];
        this.gameOver = false;
        this.gameWon = false;
        this.firstClick = true;
        this.startTime = null;
        this.timer = null;

        this.difficulties = {
            beginner: { rows: 9, cols: 9, mines: 10 },
            intermediate: { rows: 16, cols: 16, mines: 40 },
            expert: { rows: 16, cols: 30, mines: 99 }
        };

        this.currentDifficulty = 'beginner';
        this.rows = this.difficulties[this.currentDifficulty].rows;
        this.cols = this.difficulties[this.currentDifficulty].cols;
        this.mineCount = this.difficulties[this.currentDifficulty].mines;

        this.initializeGame();
        this.setupEventListeners();
    }

    initializeGame() {
        this.createBoard();
        this.placeMines();
        this.calculateNumbers();
        this.renderBoard();
        this.updateUI();
        this.resetTimer();
    }

    createBoard() {
        this.board = Array(this.rows).fill(null).map(() => Array(this.cols).fill(0));
        this.revealed = Array(this.rows).fill(null).map(() => Array(this.cols).fill(false));
        this.flagged = Array(this.rows).fill(null).map(() => Array(this.cols).fill(false));
        this.mines = [];
        this.gameOver = false;
        this.gameWon = false;
        this.firstClick = true;
    }

    placeMines() {
        let minesPlaced = 0;
        while (minesPlaced < this.mineCount) {
            const row = Math.floor(Math.random() * this.rows);
            const col = Math.floor(Math.random() * this.cols);

            if (this.board[row][col] !== -1) {
                this.board[row][col] = -1;
                this.mines.push([row, col]);
                minesPlaced++;
            }
        }
    }

    calculateNumbers() {
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                if (this.board[row][col] === -1) continue;

                let count = 0;
                for (let dr = -1; dr <= 1; dr++) {
                    for (let dc = -1; dc <= 1; dc++) {
                        if (dr === 0 && dc === 0) continue;

                        const newRow = row + dr;
                        const newCol = col + dc;

                        if (this.isValidCell(newRow, newCol) && this.board[newRow][newCol] === -1) {
                            count++;
                        }
                    }
                }
                this.board[row][col] = count;
            }
        }
    }

    isValidCell(row, col) {
        return row >= 0 && row < this.rows && col >= 0 && col < this.cols;
    }

    renderBoard() {
        const gameBoard = document.getElementById('game-board');
        gameBoard.innerHTML = '';
        gameBoard.style.gridTemplateColumns = `repeat(${this.cols}, 1fr)`;
        gameBoard.style.gridTemplateRows = `repeat(${this.rows}, 1fr)`;

        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                const cell = document.createElement('div');
                cell.className = 'cell';
                cell.dataset.row = row;
                cell.dataset.col = col;

                cell.addEventListener('click', (e) => this.handleCellClick(e, row, col));
                cell.addEventListener('contextmenu', (e) => this.handleRightClick(e, row, col));

                this.updateCellDisplay(cell, row, col);
                gameBoard.appendChild(cell);
            }
        }
    }

    updateCellDisplay(cell, row, col) {
        cell.className = 'cell';

        if (this.flagged[row][col]) {
            cell.classList.add('flagged');
        } else if (this.revealed[row][col]) {
            cell.classList.add('revealed');

            if (this.board[row][col] === -1) {
                cell.classList.add('mine');
            } else if (this.board[row][col] > 0) {
                cell.classList.add(`number-${this.board[row][col]}`);
                cell.textContent = this.board[row][col];
            }
        }
    }

    handleCellClick(e, row, col) {
        e.preventDefault();

        if (this.gameOver || this.flagged[row][col] || this.revealed[row][col]) {
            return;
        }

        if (this.firstClick) {
            this.firstClick = false;
            this.startTimer();
            this.updateGameStatus('playing');
        }

        this.revealCell(row, col);
        this.checkWinCondition();
        this.renderBoard();
    }

    handleRightClick(e, row, col) {
        e.preventDefault();

        if (this.gameOver || this.revealed[row][col]) {
            return;
        }

        this.flagged[row][col] = !this.flagged[row][col];
        this.updateUI();
        this.renderBoard();
    }

    revealCell(row, col) {
        if (!this.isValidCell(row, col) || this.revealed[row][col] || this.flagged[row][col]) {
            return;
        }

        this.revealed[row][col] = true;

        if (this.board[row][col] === -1) {
            this.gameOver = true;
            this.revealAllMines();
            this.stopTimer();
            this.updateGameStatus('lost');
            this.showGameOverModal(false);
            return;
        }

        if (this.board[row][col] === 0) {
            for (let dr = -1; dr <= 1; dr++) {
                for (let dc = -1; dc <= 1; dc++) {
                    if (dr === 0 && dc === 0) continue;
                    this.revealCell(row + dr, col + dc);
                }
            }
        }
    }

    revealAllMines() {
        for (const [row, col] of this.mines) {
            this.revealed[row][col] = true;
        }
    }

    checkWinCondition() {
        let revealedCount = 0;
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                if (this.revealed[row][col]) {
                    revealedCount++;
                }
            }
        }

        const totalCells = this.rows * this.cols;
        if (revealedCount === totalCells - this.mineCount) {
            this.gameWon = true;
            this.gameOver = true;
            this.stopTimer();
            this.updateGameStatus('won');
            this.showGameOverModal(true);
        }
    }

    setupEventListeners() {
        document.getElementById('new-game').addEventListener('click', () => {
            this.newGame();
        });

        document.getElementById('difficulty').addEventListener('change', (e) => {
            this.changeDifficulty(e.target.value);
        });

        document.getElementById('restart-game').addEventListener('click', () => {
            this.hideGameOverModal();
            this.newGame();
        });

        document.getElementById('game-over-modal').addEventListener('click', (e) => {
            if (e.target === document.getElementById('game-over-modal')) {
                this.hideGameOverModal();
            }
        });
    }

    newGame() {
        this.hideGameOverModal();
        this.initializeGame();
    }

    changeDifficulty(difficulty) {
        this.currentDifficulty = difficulty;
        this.rows = this.difficulties[this.currentDifficulty].rows;
        this.cols = this.difficulties[this.currentDifficulty].cols;
        this.mineCount = this.difficulties[this.currentDifficulty].mines;
        this.newGame();
    }

    startTimer() {
        this.startTime = Date.now();
        this.timer = setInterval(() => {
            const elapsed = Math.floor((Date.now() - this.startTime) / 1000);
            document.getElementById('timer').textContent = elapsed.toString().padStart(3, '0');
        }, 1000);
    }

    stopTimer() {
        if (this.timer) {
            clearInterval(this.timer);
            this.timer = null;
        }
    }

    resetTimer() {
        this.stopTimer();
        document.getElementById('timer').textContent = '000';
    }

    updateUI() {
        let flagCount = 0;
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                if (this.flagged[row][col]) {
                    flagCount++;
                }
            }
        }

        const remainingMines = this.mineCount - flagCount;
        document.getElementById('mine-count').textContent = remainingMines;
    }

    updateGameStatus(status) {
        const statusElement = document.getElementById('game-status');
        statusElement.className = `status-${status}`;

        const statusTexts = {
            ready: '准备开始',
            playing: '游戏进行中',
            won: '游戏胜利！',
            lost: '游戏失败！'
        };

        statusElement.textContent = statusTexts[status];
    }

    showGameOverModal(won) {
        const modal = document.getElementById('game-over-modal');
        const title = document.getElementById('game-result-title');
        const message = document.getElementById('game-result-message');

        if (won) {
            title.textContent = '恭喜你赢了！';
            title.style.color = '#27ae60';
            message.textContent = '你成功找到了所有的地雷！';
        } else {
            title.textContent = '游戏结束！';
            title.style.color = '#e74c3c';
            message.textContent = '很遗憾，你踩到了地雷！';
        }

        modal.classList.remove('hidden');
    }

    hideGameOverModal() {
        document.getElementById('game-over-modal').classList.add('hidden');
    }
}

document.addEventListener('DOMContentLoaded', () => {
    new MinesweeperGame();
});