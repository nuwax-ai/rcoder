import { Cell, CellState, GameConfig, GameState } from '../types/minesweeper';

/**
 * 创建初始游戏板
 */
export const createBoard = (config: GameConfig): Cell[][] => {
  const board: Cell[][] = [];

  for (let row = 0; row < config.rows; row++) {
    board[row] = [];
    for (let col = 0; col < config.cols; col++) {
      board[row][col] = {
        row,
        col,
        isMine: false,
        adjacentMines: 0,
        state: CellState.HIDDEN,
      };
    }
  }

  return board;
};

/**
 * 在游戏板上放置地雷
 */
export const placeMines = (board: Cell[][], config: GameConfig, excludeRow: number, excludeCol: number): void => {
  let minesPlaced = 0;
  const { rows, cols, mines } = config;

  while (minesPlaced < mines) {
    const row = Math.floor(Math.random() * rows);
    const col = Math.floor(Math.random() * cols);

    // 避免在第一次点击的位置及其周围放置地雷
    const isNearFirstClick = Math.abs(row - excludeRow) <= 1 && Math.abs(col - excludeCol) <= 1;

    if (!board[row][col].isMine && !isNearFirstClick) {
      board[row][col].isMine = true;
      minesPlaced++;
    }
  }
};

/**
 * 计算每个格子周围的地雷数量
 */
export const calculateAdjacentMines = (board: Cell[][]): void => {
  const rows = board.length;
  const cols = board[0].length;

  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      if (!board[row][col].isMine) {
        let count = 0;

        // 检查周围的8个格子
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
};

/**
 * 揭露格子
 */
export const revealCell = (board: Cell[][], row: number, col: number): Cell[][] => {
  const rows = board.length;
  const cols = board[0].length;

  if (row < 0 || row >= rows || col < 0 || col >= cols) {
    return board;
  }

  const cell = board[row][col];

  // 如果格子已经被揭露或被标记，则不处理
  if (cell.state !== CellState.HIDDEN) {
    return board;
  }

  // 揭露当前格子
  cell.state = CellState.REVEALED;

  // 如果是空格子（周围没有地雷），自动揭露周围的格子
  if (cell.adjacentMines === 0 && !cell.isMine) {
    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;
        revealCell(board, row + dr, col + dc);
      }
    }
  }

  return board;
};

/**
 * 切换格子的标记状态
 */
export const toggleFlag = (board: Cell[][], row: number, col: number): Cell[][] => {
  const cell = board[row][col];

  if (cell.state === CellState.HIDDEN) {
    cell.state = CellState.FLAGGED;
  } else if (cell.state === CellState.FLAGGED) {
    cell.state = CellState.HIDDEN;
  }

  return board;
};

/**
 * 检查游戏是否获胜
 */
export const checkWin = (board: Cell[][]): boolean => {
  for (const row of board) {
    for (const cell of row) {
      if (!cell.isMine && cell.state !== CellState.REVEALED) {
        return false;
      }
    }
  }
  return true;
};

/**
 * 检查游戏是否失败
 */
export const checkLoss = (board: Cell[][]): boolean => {
  for (const row of board) {
    for (const cell of row) {
      if (cell.isMine && cell.state === CellState.REVEALED) {
        return true;
      }
    }
  }
  return false;
};

/**
 * 获取游戏状态
 */
export const getGameState = (board: Cell[][], firstClick: boolean): GameState => {
  if (firstClick) {
    return GameState.READY;
  }

  if (checkLoss(board)) {
    return GameState.LOST;
  }

  if (checkWin(board)) {
    return GameState.WON;
  }

  return GameState.PLAYING;
};

/**
 * 揭露所有地雷（游戏结束时使用）
 */
export const revealAllMines = (board: Cell[][]): Cell[][] => {
  for (const row of board) {
    for (const cell of row) {
      if (cell.isMine && cell.state !== CellState.FLAGGED) {
        cell.state = CellState.REVEALED;
      }
    }
  }
  return board;
};