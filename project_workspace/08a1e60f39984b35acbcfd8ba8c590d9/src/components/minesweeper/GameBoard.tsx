'use client';

import { Cell } from '@/types/minesweeper';
import { CellComponent } from './Cell';

interface GameBoardProps {
  cells: Cell[][];
  onCellClick: (row: number, col: number) => void;
  onCellRightClick: (row: number, col: number) => void;
  cellSize?: 'small' | 'medium' | 'large';
}

export function GameBoard({ cells, onCellClick, onCellRightClick, cellSize = 'medium' }: GameBoardProps) {
  return (
    <div className="inline-block border-2 border-gray-600 bg-gray-200 p-2">
      <div className="grid gap-0" style={{ gridTemplateColumns: `repeat(${cells[0].length}, 1fr)` }}>
        {cells.map((row, rowIndex) =>
          row.map((cell, colIndex) => (
            <CellComponent
              key={`${rowIndex}-${colIndex}`}
              cell={cell}
              size={cellSize}
              onClick={() => onCellClick(rowIndex, colIndex)}
              onRightClick={(e) => {
                e.preventDefault();
                onCellRightClick(rowIndex, colIndex);
              }}
            />
          ))
        )}
      </div>
    </div>
  );
}