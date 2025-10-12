import { Cell, CellState, GameState, GameConfig, GameStats, MinesweeperGame, Difficulty, DIFFICULTY_CONFIGS } from '@/types/minesweeper';

export class MinesweeperEngine {
  private game: MinesweeperGame;

  constructor(config?: GameConfig, difficulty: Difficulty = 'beginner') {
    const gameConfig = config || DIFFICULTY_CONFIGS[difficulty];
    this.game = this.initializeGame(gameConfig, difficulty);
  }

  private initializeGame(config: GameConfig, difficulty: Difficulty): MinesweeperGame {
    const board = this.createEmptyBoard(config.rows, config.cols);

    return {
      config,
      board,
      gameState: 'idle',
      stats: {
        timeElapsed: 0,
        flagsPlaced: 0,
        cellsRevealed: 0,
        totalCells: config.rows * config.cols
      },
      difficulty
    };
  }

  private createEmptyBoard(rows: number, cols: number): Cell[][] {
    const board: Cell[][] = [];
    for (let row = 0; row < rows; row++) {
      board[row] = [];
      for (let col = 0; col < cols; col++) {
        board[row][col] = {
          row,
          col,
          isMine: false,
          adjacentMines: 0,
          state: 'hidden'
        };
      }
    }
    return board;
  }

  private placeMines(board: Cell[][], mines: number, excludeRow: number, excludeCol: number): void {
    const rows = board.length;
    const cols = board[0].length;
    let minesPlaced = 0;

    while (minesPlaced < mines) {
      const row = Math.floor(Math.random() * rows);
      const col = Math.floor(Math.random() * cols);

      // 避免在第一次点击的位置及其周围放置地雷
      const isExcluded = Math.abs(row - excludeRow) <= 1 && Math.abs(col - excludeCol) <= 1;

      if (!board[row][col].isMine && !isExcluded) {
        board[row][col].isMine = true;
        minesPlaced++;
      }
    }

    this.calculateAdjacentMines(board);
  }

  private calculateAdjacentMines(board: Cell[][]): void {
    const rows = board.length;
    const cols = board[0].length;

    for (let row = 0; row < rows; row++) {
      for (let col = 0; col < cols; col++) {
        if (!board[row][col].isMine) {
          board[row][col].adjacentMines = this.countAdjacentMines(board, row, col);
        }
      }
    }
  }

  private countAdjacentMines(board: Cell[][], row: number, col: number): number {
    let count = 0;
    const rows = board.length;
    const cols = board[0].length;

    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;

        const newRow = row + dr;
        const newCol = col + dc;

        if (newRow >= 0 && newRow < rows && newCol >= 0 && newCol < cols) {
          if (board[newRow][newCol].isMine) {
            count++;
          }
        }
      }
    }

    return count;
  }

  public revealCell(row: number, col: number): MinesweeperGame {
    if (this.game.gameState === 'won' || this.game.gameState === 'lost') {
      return this.game;
    }

    const cell = this.game.board[row][col];

    if (cell.state !== 'hidden') {
      return this.game;
    }

    // 如果是第一次点击，放置地雷
    if (this.game.gameState === 'idle') {
      this.placeMines(this.game.board, this.game.config.mines, row, col);
      this.game.gameState = 'playing';
    }

    cell.state = 'revealed';
    this.game.stats.cellsRevealed++;

    if (cell.isMine) {
      this.game.gameState = 'lost';
      this.revealAllMines();
      return this.game;
    }

    if (cell.adjacentMines === 0) {
      this.revealAdjacentCells(row, col);
    }

    // 检查是否获胜
    if (this.checkWin()) {
      this.game.gameState = 'won';
    }

    return this.game;
  }

  private revealAdjacentCells(row: number, col: number): void {
    const rows = this.game.board.length;
    const cols = this.game.board[0].length;

    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;

        const newRow = row + dr;
        const newCol = col + dc;

        if (newRow >= 0 && newRow < rows && newCol >= 0 && newCol < cols) {
          const adjacentCell = this.game.board[newRow][newCol];
          if (adjacentCell.state === 'hidden' && !adjacentCell.isMine) {
            adjacentCell.state = 'revealed';
            this.game.stats.cellsRevealed++;

            if (adjacentCell.adjacentMines === 0) {
              this.revealAdjacentCells(newRow, newCol);
            }
          }
        }
      }
    }
  }

  private revealAllMines(): void {
    for (let row = 0; row < this.game.board.length; row++) {
      for (let col = 0; col < this.game.board[0].length; col++) {
        const cell = this.game.board[row][col];
        if (cell.isMine && cell.state !== 'flagged') {
          cell.state = 'revealed';
        }
      }
    }
  }

  private checkWin(): boolean {
    const totalSafeCells = this.game.stats.totalCells - this.game.config.mines;
    return this.game.stats.cellsRevealed === totalSafeCells;
  }

  public toggleFlag(row: number, col: number): MinesweeperGame {
    if (this.game.gameState === 'won' || this.game.gameState === 'lost') {
      return this.game;
    }

    const cell = this.game.board[row][col];

    if (cell.state === 'revealed') {
      return this.game;
    }

    switch (cell.state) {
      case 'hidden':
        cell.state = 'flagged';
        this.game.stats.flagsPlaced++;
        break;
      case 'flagged':
        cell.state = 'questioned';
        this.game.stats.flagsPlaced--;
        break;
      case 'questioned':
        cell.state = 'hidden';
        break;
    }

    return this.game;
  }

  public restart(): MinesweeperGame {
    this.game = this.initializeGame(this.game.config, this.game.difficulty);
    return this.game;
  }

  public changeDifficulty(difficulty: Difficulty): MinesweeperGame {
    const config = DIFFICULTY_CONFIGS[difficulty];
    this.game = this.initializeGame(config, difficulty);
    return this.game;
  }

  public getGame(): MinesweeperGame {
    return this.game;
  }

  public updateTime(elapsedTime: number): MinesweeperGame {
    this.game.stats.timeElapsed = elapsedTime;
    return this.game;
  }

  public getRemainingMines(): number {
    return this.game.config.mines - this.game.stats.flagsPlaced;
  }
}