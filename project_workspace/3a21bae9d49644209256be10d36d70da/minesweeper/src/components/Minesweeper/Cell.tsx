'use client';

import React from 'react';
import { Cell } from '@/types/minesweeper';

interface CellProps {
  cell: Cell;
  onClick: (row: number, col: number) => void;
  onRightClick: (row: number, col: number) => void;
  isGameLost: boolean;
}

export const MinesweeperCell: React.FC<CellProps> = ({
  cell,
  onClick,
  onRightClick,
  isGameLost
}) => {
  const getCellContent = () => {
    switch (cell.state) {
      case 'hidden':
        return '';
      case 'flagged':
        return '🚩';
      case 'questioned':
        return '❓';
      case 'revealed':
        if (cell.isMine) {
          return isGameLost ? '💣' : '';
        }
        return cell.adjacentMines > 0 ? cell.adjacentMines.toString() : '';
      default:
        return '';
    }
  };

  const getCellStyle = () => {
    let baseStyle = "w-8 h-8 border border-gray-400 flex items-center justify-center text-sm font-bold cursor-pointer transition-all duration-100 ";

    switch (cell.state) {
      case 'hidden':
        baseStyle += "bg-gray-300 hover:bg-gray-200 ";
        break;
      case 'flagged':
        baseStyle += "bg-gray-300 ";
        break;
      case 'questioned':
        baseStyle += "bg-gray-300 ";
        break;
      case 'revealed':
        if (cell.isMine) {
          baseStyle += isGameLost ? "bg-red-500 " : "bg-red-300 ";
        } else {
          baseStyle += "bg-white ";
        }
        break;
    }

    // 为不同数字设置不同颜色
    if (cell.state === 'revealed' && !cell.isMine && cell.adjacentMines > 0) {
      const colorMap: { [key: number]: string } = {
        1: 'text-blue-600',
        2: 'text-green-600',
        3: 'text-red-600',
        4: 'text-purple-600',
        5: 'text-orange-600',
        6: 'text-cyan-600',
        7: 'text-black',
        8: 'text-gray-600'
      };
      baseStyle += colorMap[cell.adjacentMines] || 'text-gray-800';
    }

    return baseStyle;
  };

  const handleClick = (e: React.MouseEvent) => {
    e.preventDefault();
    if (cell.state !== 'revealed') {
      onClick(cell.row, cell.col);
    }
  };

  const handleRightClick = (e: React.MouseEvent) => {
    e.preventDefault();
    onRightClick(cell.row, cell.col);
  };

  return (
    <div
      className={getCellStyle()}
      onClick={handleClick}
      onContextMenu={handleRightClick}
      role="button"
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          handleClick(e as any);
        }
        if (e.key === 'ContextMenu' || (e.key === 'f' && e.ctrlKey)) {
          e.preventDefault();
          handleRightClick(e as any);
        }
      }}
    >
      {getCellContent()}
    </div>
  );
};