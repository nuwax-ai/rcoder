import React, { useState } from 'react';
import { Cell as CellType, CellState } from '../types/minesweeper';

interface CellProps {
  cell: CellType;
  onClick: (row: number, col: number) => void;
  onRightClick: (row: number, col: number) => void;
  onDoubleClick: (row: number, col: number) => void;
  isGameLost: boolean;
}

export const Cell: React.FC<CellProps> = ({ cell, onClick, onRightClick, onDoubleClick, isGameLost }) => {
  const [isPressed, setIsPressed] = useState(false);

  const handleMouseDown = (e: React.MouseEvent) => {
    if (e.button === 0) { // 左键
      setIsPressed(true);
    }
  };

  const handleMouseUp = () => {
    setIsPressed(false);
  };

  const handleMouseLeave = () => {
    setIsPressed(false);
  };

  const handleClick = (e: React.MouseEvent) => {
    e.preventDefault();
    if (cell.state === CellState.REVEALED) {
      return;
    }
    onClick(cell.row, cell.col);
  };

  const handleContextMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    onRightClick(cell.row, cell.col);
  };

  const handleDoubleClickEvent = (e: React.MouseEvent) => {
    e.preventDefault();
    if (cell.state === CellState.REVEALED) {
      onDoubleClick(cell.row, cell.col);
    }
  };

  const getCellContent = () => {
    if (cell.state === CellState.HIDDEN) {
      return null;
    }

    if (cell.state === CellState.FLAGGED) {
      return '🚩';
    }

    if (cell.isMine) {
      return isGameLost ? '💣' : null;
    }

    if (cell.adjacentMines > 0) {
      return cell.adjacentMines;
    }

    return null;
  };

  const getCellClassName = () => {
    const baseClasses = 'w-8 h-8 border border-gray-400 flex items-center justify-center text-sm font-bold cursor-pointer select-none transition-all duration-100';

    if (cell.state === CellState.REVEALED) {
      if (cell.isMine) {
        return `${baseClasses} bg-red-500 text-white`;
      }
      return `${baseClasses} bg-gray-200`;
    }

    if (cell.state === CellState.FLAGGED) {
      return `${baseClasses} bg-blue-200`;
    }

    if (isPressed) {
      return `${baseClasses} bg-gray-300`;
    }

    return `${baseClasses} bg-gray-400 hover:bg-gray-350`;
  };

  const getNumberColor = () => {
    if (!cell.isMine && cell.adjacentMines > 0) {
      const colors = [
        '', // 0 - 不显示
        'cell-number-1', // 1
        'cell-number-2', // 2
        'cell-number-3', // 3
        'cell-number-4', // 4
        'cell-number-5', // 5
        'cell-number-6', // 6
        'cell-number-7', // 7
        'cell-number-8', // 8
      ];
      return colors[cell.adjacentMines] || '';
    }
    return '';
  };

  return (
    <div
      className={`${getCellClassName()} ${getNumberColor()}`}
      onClick={handleClick}
      onContextMenu={handleContextMenu}
      onDoubleClick={handleDoubleClickEvent}
      onMouseDown={handleMouseDown}
      onMouseUp={handleMouseUp}
      onMouseLeave={handleMouseLeave}
      role="button"
      tabIndex={0}
      aria-label={`Cell ${cell.row},${cell.col}, ${cell.state}`}
    >
      {getCellContent()}
    </div>
  );
};