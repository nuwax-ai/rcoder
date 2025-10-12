import { Cell, GameBoard, GameConfig, GameState } from '@/types/minesweeper';

// 创建空的游戏板
export function createEmptyBoard(rows: number, cols: number): Cell[][] {
  return Array.from({ length: rows }, () =>
    Array.from({ length: cols }, () => ({
      isMine: false,
      state: 'hidden' as const,
      adjacentMines: 0,
    }))
  );
}

// 放置地雷
export function placeMines(board: Cell[][], mines: number, excludeRow: number, excludeCol: number): void {
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
}

// 计算每个格子周围的地雷数量
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

// 初始化游戏板
export function initializeBoard(config: GameConfig, firstClickRow: number, firstClickCol: number): Cell[][] {
  const board = createEmptyBoard(config.rows, config.cols);
  placeMines(board, config.mines, firstClickRow, firstClickCol);
  calculateAdjacentMines(board);
  return board;
}

// 揭示格子
export function revealCell(board: Cell[][], row: number, col: number): boolean {
  const rows = board.length;
  const cols = board[0].length;

  if (row < 0 || row >= rows || col < 0 || col >= cols) {
    return false;
  }

  const cell = board[row][col];

  if (cell.state !== 'hidden') {
    return false;
  }

  cell.state = 'revealed';

  // 如果是地雷，游戏结束
  if (cell.isMine) {
    return true;
  }

  // 如果周围没有地雷，递归揭示相邻格子
  if (cell.adjacentMines === 0) {
    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;
        revealCell(board, row + dr, col + dc);
      }
    }
  }

  return false;
}

// 切换旗帜状态
export function toggleFlag(board: Cell[][], row: number, col: number): boolean {
  const rows = board.length;
  const cols = board[0].length;

  if (row < 0 || row >= rows || col < 0 || col >= cols) {
    return false;
  }

  const cell = board[row][col];

  if (cell.state === 'revealed') {
    return false;
  }

  cell.state = cell.state === 'hidden' ? 'flagged' : 'hidden';
  return true;
}

// 检查游戏是否获胜
export function checkWin(board: Cell[][]): boolean {
  const rows = board.length;
  const cols = board[0].length;

  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      const cell = board[row][col];

      // 如果有非地雷格子未被揭示，游戏未获胜
      if (!cell.isMine && cell.state !== 'revealed') {
        return false;
      }
    }
  }

  return true;
}

// 揭示所有地雷
export function revealAllMines(board: Cell[][]): void {
  const rows = board.length;
  const cols = board[0].length;

  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      if (board[row][col].isMine) {
        board[row][col].state = 'revealed';
      }
    }
  }
}

// 计算游戏统计信息
export function calculateGameStats(board: Cell[][]): {
  revealedCells: number;
  flaggedCells: number;
  remainingMines: number;
} {
  const rows = board.length;
  const cols = board[0].length;
  let revealedCells = 0;
  let flaggedCells = 0;
  let totalMines = 0;

  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      const cell = board[row][col];

      if (cell.isMine) {
        totalMines++;
      }

      if (cell.state === 'revealed') {
        revealedCells++;
      } else if (cell.state === 'flagged') {
        flaggedCells++;
      }
    }
  }

  return {
    revealedCells,
    flaggedCells,
    remainingMines: totalMines - flaggedCells,
  };
}