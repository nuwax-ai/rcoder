/**
 * 扫雷游戏板组件
 */

import React, { memo } from 'react';
import { GameData, GameState } from '../../types/minesweeper';
import CellComponent from './Cell';

interface GameBoardProps {
  gameData: GameData;
  onCellClick: (row: number, col: number) => void;
  onCellRightClick: (row: number, col: number) => void;
}

const GameBoard: React.FC<GameBoardProps> = memo(({
  gameData,
  onCellClick,
  onCellRightClick
}) => {
  const { board, state } = gameData;
  const isDisabled = state === GameState.WON || state === GameState.LOST;

  return (
    <div className="inline-block bg-gray-200 p-2 rounded-lg shadow-lg">
      <div
        className="grid gap-0"
        style={{
          gridTemplateColumns: `repeat(${board[0]?.length || 0}, minmax(0, 1fr))`,
        }}
      >
        {board.map((row, rowIndex) =>
          row.map((cell, colIndex) => (
            <CellComponent
              key={`${rowIndex}-${colIndex}`}
              cell={cell}
              onClick={onCellClick}
              onRightClick={onCellRightClick}
              disabled={isDisabled}
            />
          ))
        )}
      </div>
    </div>
  );
});

GameBoard.displayName = 'GameBoard';

export default GameBoard;