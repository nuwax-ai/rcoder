/**
 * 扫雷游戏单元格组件
 */

import React, { memo } from 'react';
import { Cell, CellState } from '../../types/minesweeper';

interface CellComponentProps {
  cell: Cell;
  onClick: (row: number, col: number) => void;
  onRightClick: (row: number, col: number) => void;
  disabled?: boolean;
}

const CellComponent: React.FC<CellComponentProps> = memo(({
  cell,
  onClick,
  onRightClick,
  disabled = false
}) => {
  const handleClick = (e: React.MouseEvent) => {
    e.preventDefault();
    if (!disabled && cell.state === CellState.HIDDEN) {
      onClick(cell.row, cell.col);
    }
  };

  const handleRightClick = (e: React.MouseEvent) => {
    e.preventDefault();
    if (!disabled && cell.state !== CellState.REVEALED) {
      onRightClick(cell.row, cell.col);
    }
  };

  const getCellContent = () => {
    switch (cell.state) {
      case CellState.HIDDEN:
        return '';
      case CellState.FLAGGED:
        return '🚩';
      case CellState.REVEALED:
        if (cell.isMine) {
          return '💣';
        }
        return cell.adjacentMines > 0 ? cell.adjacentMines.toString() : '';
      default:
        return '';
    }
  };

  const getCellStyle = () => {
    const baseStyle = "w-8 h-8 border border-gray-400 flex items-center justify-center text-sm font-bold cursor-pointer transition-all duration-150 select-none";

    switch (cell.state) {
      case CellState.HIDDEN:
        return `${baseStyle} bg-gray-300 hover:bg-gray-200`;
      case CellState.FLAGGED:
        return `${baseStyle} bg-gray-300`;
      case CellState.REVEALED:
        if (cell.isMine) {
          return `${baseStyle} bg-red-500 text-white`;
        }
        return `${baseStyle} bg-white ${getCellTextColor()}`;
      default:
        return baseStyle;
    }
  };

  const getCellTextColor = () => {
    const colors = [
      '',           // 0 - no text
      'text-blue-600',   // 1
      'text-green-600',  // 2
      'text-red-600',    // 3
      'text-purple-600', // 4
      'text-yellow-600', // 5
      'text-pink-600',   // 6
      'text-gray-900',   // 7
      'text-gray-700',   // 8
    ];
    return colors[cell.adjacentMines] || '';
  };

  return (
    <button
      className={getCellStyle()}
      onClick={handleClick}
      onContextMenu={handleRightClick}
      disabled={disabled}
    >
      {getCellContent()}
    </button>
  );
});

CellComponent.displayName = 'CellComponent';

export default CellComponent;