export type CellState = 'hidden' | 'revealed' | 'flagged';

export interface Cell {
  isMine: boolean;
  state: CellState;
  adjacentMines: number;
}

export type GameState = 'idle' | 'playing' | 'won' | 'lost';

export interface GameConfig {
  rows: number;
  cols: number;
  mines: number;
}

export interface GameBoard {
  cells: Cell[][];
  gameState: GameState;
  remainingMines: number;
  revealedCells: number;
  flaggedCells: number;
}