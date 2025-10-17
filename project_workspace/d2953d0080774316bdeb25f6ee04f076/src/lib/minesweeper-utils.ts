/**
 * 扫雷游戏工具函数
 */

import { Cell, CellState, GameConfig, GameData, GameState, GameStats, Difficulty, DEFAULT_CONFIGS } from '../types/minesweeper';

/**
 * 创建新的游戏板
 */
export function createBoard(config: GameConfig): Cell[][] {
  const board: Cell[][] = [];

  for (let row = 0; row < config.rows; row++) {
    board[row] = [];
    for (let col = 0; col < config.cols; col++) {
      board[row][col] = {
        row,
        col,
        isMine: false,
        state: CellState.HIDDEN,
        adjacentMines: 0,
      };
    }
  }

  return board;
}

/**
 * 在游戏板中放置地雷
 */
export function placeMines(board: Cell[][], config: GameConfig, excludeRow: number, excludeCol: number): void {
  let minesPlaced = 0;

  while (minesPlaced < config.mines) {
    const row = Math.floor(Math.random() * config.rows);
    const col = Math.floor(Math.random() * config.cols);

    // 避免在第一次点击的位置及其周围放置地雷
    const isExcluded = Math.abs(row - excludeRow) <= 1 && Math.abs(col - excludeCol) <= 1;

    if (!board[row][col].isMine && !isExcluded) {
      board[row][col].isMine = true;
      minesPlaced++;
    }
  }
}

/**
 * 计算每个格子周围的地雷数量
 */
export function calculateAdjacentMines(board: Cell[][]): void {
  const rows = board.length;
  const cols = board[0].length;

  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      if (!board[row][col].isMine) {
        let count = 0;

        // 检查8个相邻格子
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

        board[row][col].adjacentMines = count;
      }
    }
  }
}

/**
 * 创建新的游戏数据
 */
export function createNewGame(config: GameConfig): GameData {
  const board = createBoard(config);

  return {
    board,
    config,
    state: GameState.READY,
    stats: {
      time: 0,
      flagsLeft: config.mines,
      cellsRevealed: 0,
      totalCells: config.rows * config.cols,
    },
    startTime: null,
    endTime: null,
  };
}

/**
 * 开始游戏（第一次点击）
 */
export function startGame(gameData: GameData, firstClickRow: number, firstClickCol: number): GameData {
  const newBoard = createBoard(gameData.config);

  // 放置地雷（避开第一次点击位置）
  placeMines(newBoard, gameData.config, firstClickRow, firstClickCol);

  // 计算周围地雷数量
  calculateAdjacentMines(newBoard);

  return {
    ...gameData,
    board: newBoard,
    state: GameState.PLAYING,
    startTime: Date.now(),
    endTime: null,
  };
}

/**
 * 翻开格子
 */
export function revealCell(gameData: GameData, row: number, col: number): GameData {
  const newBoard = gameData.board.map(row => row.map(cell => ({ ...cell })));
  const cell = newBoard[row][col];

  if (cell.state !== CellState.HIDDEN || cell.isMine) {
    return gameData;
  }

  cell.state = CellState.REVEALED;

  const newStats = {
    ...gameData.stats,
    cellsRevealed: gameData.stats.cellsRevealed + 1,
  };

  // 如果是空格子（周围没有地雷），自动翻开周围的格子
  if (cell.adjacentMines === 0) {
    const rows = newBoard.length;
    const cols = newBoard[0].length;

    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;

        const newRow = row + dr;
        const newCol = col + dc;

        if (newRow >= 0 && newRow < rows && newCol >= 0 && newCol < cols) {
          const adjacentCell = newBoard[newRow][newCol];
          if (adjacentCell.state === CellState.HIDDEN && !adjacentCell.isMine) {
            // 递归翻开相邻格子
            const result = revealCell({ ...gameData, board: newBoard, stats: newStats }, newRow, newCol);
            return result;
          }
        }
      }
    }
  }

  return {
    ...gameData,
    board: newBoard,
    stats: newStats,
  };
}

/**
 * 切换旗帜标记
 */
export function toggleFlag(gameData: GameData, row: number, col: number): GameData {
  const newBoard = gameData.board.map(row => row.map(cell => ({ ...cell })));
  const cell = newBoard[row][col];

  if (cell.state === CellState.REVEALED) {
    return gameData;
  }

  const newState = cell.state === CellState.HIDDEN ? CellState.FLAGGED : CellState.HIDDEN;
  cell.state = newState;

  const flagsUsed = newBoard.flat().filter(c => c.state === CellState.FLAGGED).length;

  return {
    ...gameData,
    board: newBoard,
    stats: {
      ...gameData.stats,
      flagsLeft: gameData.config.mines - flagsUsed,
    },
  };
}

/**
 * 检查游戏是否胜利
 */
export function checkWin(gameData: GameData): boolean {
  const { board, config } = gameData;
  const totalCells = config.rows * config.cols;
  const nonMineCells = totalCells - config.mines;
  const revealedCells = board.flat().filter(cell => cell.state === CellState.REVEALED).length;

  return revealedCells === nonMineCells;
}

/**
 * 翻开所有地雷（游戏结束时）
 */
export function revealAllMines(gameData: GameData): GameData {
  const newBoard = gameData.board.map(row => row.map(cell => {
    if (cell.isMine && cell.state !== CellState.FLAGGED) {
      return { ...cell, state: CellState.REVEALED };
    }
    return cell;
  }));

  return {
    ...gameData,
    board: newBoard,
  };
}

/**
 * 获取游戏配置
 */
export function getGameConfig(difficulty: Difficulty, customConfig?: Partial<GameConfig>): GameConfig {
  if (difficulty === Difficulty.CUSTOM && customConfig) {
    return {
      ...customConfig,
      rows: customConfig.rows || 16,
      cols: customConfig.cols || 16,
      mines: customConfig.mines || 40,
      difficulty,
    };
  }

  return {
    ...DEFAULT_CONFIGS[difficulty],
    difficulty,
  };
}

/**
 * 格式化时间显示
 */
export function formatTime(seconds: number): string {
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}`;
}