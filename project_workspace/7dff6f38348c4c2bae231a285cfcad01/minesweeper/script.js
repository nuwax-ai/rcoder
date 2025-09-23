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
        this.gameOver = false;
        this.gameWon = false;
        this.firstClick = true;
        
        this.initGame();
        this.setupEventListeners();
    }
    
    initGame() {
        const config = this.difficulties[this.currentDifficulty];
        this.rows = config.rows;
        this.cols = config.cols;
        this.mines = config.mines;
        
        this.board = Array(this.rows).fill().map(() => Array(this.cols).fill(0));
        this.revealed = Array(this.rows).fill().map(() => Array(this.cols).fill(false));
        this.flagged = Array(this.rows).fill().map(() => Array(this.cols).fill(false));
        this.gameOver = false;
        this.gameWon = false;
        this.firstClick = true;
        
        this.updateUI();
        this.renderBoard();
    }
    
    placeMines(excludeRow, excludeCol) {
        let minesPlaced = 0;
        
        while (minesPlaced < this.mines) {
            const row = Math.floor(Math.random() * this.rows);
            const col = Math.floor(Math.random() * this.cols);
            
            if (this.board[row][col] !== -1 && !(row === excludeRow && col === excludeCol)) {
                this.board[row][col] = -1;
                minesPlaced++;
                
                for (let dr = -1; dr <= 1; dr++) {
                    for (let dc = -1; dc <= 1; dc++) {
                        const newRow = row + dr;
                        const newCol = col + dc;
                        
                        if (this.isValidCell(newRow, newCol) && this.board[newRow][newCol] !== -1) {
                            this.board[newRow][newCol]++;
                        }
                    }
                }
            }
        }
    }
    
    isValidCell(row, col) {
        return row >= 0 && row < this.rows && col >= 0 && col < this.cols;
    }
    
    revealCell(row, col) {
        if (!this.isValidCell(row, col) || this.revealed[row][col] || this.flagged[row][col] || this.gameOver) {
            return;
        }
        
        if (this.firstClick) {
            this.placeMines(row, col);
            this.firstClick = false;
        }
        
        this.revealed[row][col] = true;
        
        if (this.board[row][col] === -1) {
            this.gameOver = true;
            this.revealAllMines();
            this.showMessage('游戏结束！你踩到地雷了！', false);
            return;
        }
        
        if (this.board[row][col] === 0) {
            for (let dr = -1; dr <= 1; dr++) {
                for (let dc = -1; dc <= 1; dc++) {
                    this.revealCell(row + dr, col + dc);
                }
            }
        }
        
        this.checkWin();
        this.renderBoard();
    }
    
    toggleFlag(row, col) {
        if (!this.isValidCell(row, col) || this.revealed[row][col] || this.gameOver) {
            return;
        }
        
        this.flagged[row][col] = !this.flagged[row][col];
        this.updateUI();
        this.renderBoard();
    }
    
    revealAllMines() {
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                if (this.board[row][col] === -1) {
                    this.revealed[row][col] = true;
                }
            }
        }
    }
    
    checkWin() {
        let revealedCount = 0;
        
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                if (this.revealed[row][col]) {
                    revealedCount++;
                }
            }
        }
        
        if (revealedCount === this.rows * this.cols - this.mines) {
            this.gameWon = true;
            this.gameOver = true;
            this.showMessage('恭喜！你赢了！', true);
        }
    }
    
    renderBoard() {
        const boardElement = document.getElementById('game-board');
        boardElement.innerHTML = '';
        boardElement.style.gridTemplateColumns = `repeat(${this.cols}, 1fr)`;
        
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                const cell = document.createElement('div');
                cell.className = 'cell';
                cell.dataset.row = row;
                cell.dataset.col = col;
                
                if (this.revealed[row][col]) {
                    cell.classList.add('revealed');
                    
                    if (this.board[row][col] === -1) {
                        cell.classList.add('mine');
                        cell.textContent = '💣';
                    } else if (this.board[row][col] > 0) {
                        cell.textContent = this.board[row][col];
                        cell.classList.add(`number-${this.board[row][col]}`);
                    }
                } else if (this.flagged[row][col]) {
                    cell.classList.add('flagged');
                    cell.textContent = '🚩';
                }
                
                cell.addEventListener('click', (e) => this.handleCellClick(e));
                cell.addEventListener('contextmenu', (e) => this.handleRightClick(e));
                
                boardElement.appendChild(cell);
            }
        }
    }
    
    handleCellClick(event) {
        const row = parseInt(event.target.dataset.row);
        const col = parseInt(event.target.dataset.col);
        this.revealCell(row, col);
    }
    
    handleRightClick(event) {
        event.preventDefault();
        const row = parseInt(event.target.dataset.row);
        const col = parseInt(event.target.dataset.col);
        this.toggleFlag(row, col);
    }
    
    updateUI() {
        document.getElementById('mine-count').textContent = this.mines;
        
        let flagCount = 0;
        for (let row = 0; row < this.rows; row++) {
            for (let col = 0; col < this.cols; col++) {
                if (this.flagged[row][col]) {
                    flagCount++;
                }
            }
        }
        
        document.getElementById('flag-count').textContent = this.mines - flagCount;
    }
    
    showMessage(message, isWin) {
        const messageElement = document.getElementById('game-message');
        messageElement.textContent = message;
        messageElement.className = `game-message ${isWin ? 'win' : 'lose'}`;
        messageElement.classList.remove('hidden');
    }
    
    hideMessage() {
        document.getElementById('game-message').classList.add('hidden');
    }
    
    setDifficulty(difficulty) {
        this.currentDifficulty = difficulty;
        document.querySelectorAll('.difficulty-btn').forEach(btn => {
            btn.classList.remove('active');
        });
        document.getElementById(difficulty).classList.add('active');
        this.initGame();
        this.hideMessage();
    }
    
    setupEventListeners() {
        document.getElementById('restart').addEventListener('click', () => {
            this.initGame();
            this.hideMessage();
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
    }
}

document.addEventListener('DOMContentLoaded', () => {
    new Minesweeper();
});