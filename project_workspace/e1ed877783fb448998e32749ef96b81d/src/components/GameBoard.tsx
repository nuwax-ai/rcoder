import React from 'react';
import { Cell as CellType } from '../types/minesweeper';
import { Cell } from './Cell';

interface GameBoardProps {
  board: CellType[][];
  onCellClick: (row: number, col: number) => void;
  onRightClick: (row: number, col: number) => void;
  onDoubleClick: (row: number, col: number) => void;
  isGameLost: boolean;
}

export const GameBoard: React.FC<GameBoardProps> = ({
  board,
  onCellClick,
  onRightClick,
  onDoubleClick,
  isGameLost,
}) => {
  return (
    <div className="inline-block border-2 border-gray-600 bg-gray-100">
      {board.map((row, rowIndex) => (
        <div key={rowIndex} className="flex">
          {row.map((cell) => (
            <Cell
              key={`${cell.row}-${cell.col}`}
              cell={cell}
              onClick={onCellClick}
              onRightClick={onRightClick}
              onDoubleClick={onDoubleClick}
              isGameLost={isGameLost}
            />
          ))}
        </div>
      ))}
    </div>
  );
};