'use client';

import React from 'react';
import { MinesweeperGame } from '@/types/minesweeper';
import { MinesweeperCell } from './Cell';

interface GameBoardProps {
  game: MinesweeperGame;
  onCellClick: (row: number, col: number) => void;
  onCellRightClick: (row: number, col: number) => void;
}

export const MinesweeperGameBoard: React.FC<GameBoardProps> = ({
  game,
  onCellClick,
  onCellRightClick
}) => {
  return (
    <div className="inline-block bg-gray-100 p-2 rounded-lg shadow-lg">
      <div
        className="grid gap-0"
        style={{
          gridTemplateColumns: `repeat(${game.config.cols}, minmax(0, 1fr))`,
        }}
      >
        {game.board.map((row, rowIndex) =>
          row.map((cell, colIndex) => (
            <MinesweeperCell
              key={`${rowIndex}-${colIndex}`}
              cell={cell}
              onClick={onCellClick}
              onRightClick={onCellRightClick}
              isGameLost={game.gameState === 'lost'}
            />
          ))
        )}
      </div>
    </div>
  );
};