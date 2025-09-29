class Minesweeper {
    constructor() {
        this.difficulties = {
            easy: { rows: 9, cols: 9, mines: 10 },
            medium: { rows: 16, cols: 16, mines: 40 },
            hard: { rows: 16, cols: 30, mines: 99 }
        };

        this.currentDifficulty = 'easy';
        this.board = [];
        this.revealed = [];
        this.flagged = [];
        this.mines = [];
        this.gameOver = false;
        this.gameWon = false;
        this.timer = 0;
        this.timerInterval = null;

        this.initGame();
        this.bindEvents();
    }

    initGame() {
        const config = this.difficulties[this.currentDifficulty];
        this.rows = config.rows;
        this.cols = config.cols;
        this.mineCount = config.mines;
        this.gameOver = false;
        this.gameWon = false;
        this.timer = 0;

        this.clearTimer();
        this.resetBoard();
        this.createBoard();
        this.placeMines();
        this.calculateNumbers();
        this.renderBoard();
        this.updateUI();
    }

    resetBoard() {
        this.board = [];
        this.revealed = [];
        this.flagged = [];
        this.mines = [];

        for (let i = 0; i < this.rows; i++) {
            this.board[i] = [];
            this.revealed[i] = [];
            this.flagged[i] = [];
            for (let j = 0; j < this.cols; j++) {
                this.board[i][j] = 0;
                this.revealed[i][j] = false;
                this.flagged[i][j] = false;
            }
        }
    }

    createBoard() {
        const gameBoard = document.getElementById('game-board');
        gameBoard.innerHTML = '';
        gameBoard.style.gridTemplateColumns = `repeat(${this.cols}, 1fr)`;
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
        for (let i = 0; i < this.rows; i++) {
            for (let j = 0; j < this.cols; j++) {
                if (this.board[i][j] !== -1) {
                    this.board[i][j] = this.countAdjacentMines(i, j);
                }
            }
        }
    }

    countAdjacentMines(row, col) {
        let count = 0;
        for (let i = -1; i <= 1; i++) {
            for (let j = -1; j <= 1; j++) {
                const newRow = row + i;
                const newCol = col + j;
                if (this.isValidCell(newRow, newCol) && this.board[newRow][newCol] === -1) {
                    count++;
                }
            }
        }
        return count;
    }

    isValidCell(row, col) {
        return row >= 0 && row < this.rows && col >= 0 && col < this.cols;
    }

    renderBoard() {
        const gameBoard = document.getElementById('game-board');
        gameBoard.innerHTML = '';

        for (let i = 0; i < this.rows; i++) {
            for (let j = 0; j < this.cols; j++) {
                const cell = document.createElement('div');
                cell.className = 'cell';
                cell.dataset.row = i;
                cell.dataset.col = j;

                if (this.revealed[i][j]) {
                    cell.classList.add('revealed');
                    if (this.board[i][j] === -1) {
                        cell.classList.add('mine');
                    } else if (this.board[i][j] > 0) {
                        cell.textContent = this.board[i][j];
                        cell.classList.add(`number-${this.board[i][j]}`);
                    }
                } else if (this.flagged[i][j]) {
                    cell.classList.add('flagged');
                }

                cell.addEventListener('click', (e) => this.handleCellClick(e));
                cell.addEventListener('contextmenu', (e) => this.handleRightClick(e));

                gameBoard.appendChild(cell);
            }
        }
    }

    handleCellClick(event) {
        if (this.gameOver) return;

        const row = parseInt(event.target.dataset.row);
        const col = parseInt(event.target.dataset.col);

        if (this.flagged[row][col] || this.revealed[row][col]) return;

        if (this.timer === 0) {
            this.startTimer();
        }

        this.revealCell(row, col);
        this.renderBoard();
        this.checkWinCondition();
    }

    handleRightClick(event) {
        event.preventDefault();
        if (this.gameOver) return;

        const row = parseInt(event.target.dataset.row);
        const col = parseInt(event.target.dataset.col);

        if (this.revealed[row][col]) return;

        this.flagged[row][col] = !this.flagged[row][col];
        this.renderBoard();
        this.updateUI();
    }

    revealCell(row, col) {
        if (!this.isValidCell(row, col) || this.revealed[row][col] || this.flagged[row][col]) {
            return;
        }

        this.revealed[row][col] = true;

        if (this.board[row][col] === -1) {
            this.gameOver = true;
            this.clearTimer();
            this.revealAllMines();
            this.showMessage('游戏结束！你踩到地雷了！', 'lose');
            return;
        }

        if (this.board[row][col] === 0) {
            for (let i = -1; i <= 1; i++) {
                for (let j = -1; j <= 1; j++) {
                    this.revealCell(row + i, col + j);
                }
            }
        }
    }

    revealAllMines() {
        for (let i = 0; i < this.rows; i++) {
            for (let j = 0; j < this.cols; j++) {
                if (this.board[i][j] === -1) {
                    this.revealed[i][j] = true;
                }
            }
        }
    }

    checkWinCondition() {
        let revealedCount = 0;
        for (let i = 0; i < this.rows; i++) {
            for (let j = 0; j < this.cols; j++) {
                if (this.revealed[i][j]) {
                    revealedCount++;
                }
            }
        }

        if (revealedCount === this.rows * this.cols - this.mineCount) {
            this.gameWon = true;
            this.gameOver = true;
            this.clearTimer();
            this.showMessage('恭喜你赢了！', 'win');
        }
    }

    updateUI() {
        const flaggedCount = this.flagged.flat().filter(f => f).length;
        document.getElementById('mine-count').textContent = this.mineCount - flaggedCount;
        document.getElementById('timer').textContent = this.timer;
    }

    startTimer() {
        this.timerInterval = setInterval(() => {
            this.timer++;
            this.updateUI();
        }, 1000);
    }

    clearTimer() {
        if (this.timerInterval) {
            clearInterval(this.timerInterval);
            this.timerInterval = null;
        }
    }

    showMessage(message, type) {
        const messageElement = document.getElementById('game-message');
        messageElement.textContent = message;
        messageElement.className = `game-message ${type}`;
    }

    setDifficulty(difficulty) {
        this.currentDifficulty = difficulty;
        this.initGame();
    }

    bindEvents() {
        document.getElementById('new-game').addEventListener('click', () => {
            this.initGame();
        });

        document.getElementById('easy').addEventListener('click', () => {
            this.setDifficulty('easy');
        });

        document.getElementById('medium').addEventListener('click', () => {
            this.setDifficulty('medium');
        });

        document.getElementById('hard').addEventListener('click', () => {
            this.setDifficulty('hard');
        });

        document.addEventListener('contextmenu', (e) => {
            if (e.target.classList.contains('cell')) {
                e.preventDefault();
            }
        });
    }
}

document.addEventListener('DOMContentLoaded', () => {
    new Minesweeper();
});