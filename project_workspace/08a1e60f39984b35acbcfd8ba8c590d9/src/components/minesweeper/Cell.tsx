'use client';

import { Cell } from '@/types/minesweeper';
import { cn } from '@/lib/utils';

interface CellComponentProps {
  cell: Cell;
  onClick: () => void;
  onRightClick: (e: React.MouseEvent) => void;
  size?: 'small' | 'medium' | 'large';
}

export function CellComponent({ cell, onClick, onRightClick, size = 'medium' }: CellComponentProps) {
  const sizeClasses = {
    small: 'w-6 h-6 text-xs',
    medium: 'w-8 h-8 text-sm',
    large: 'w-10 h-10 text-base',
  };

  const getCellContent = () => {
    if (cell.state === 'flagged') {
      return '🚩';
    }

    if (cell.state !== 'revealed') {
      return '';
    }

    if (cell.isMine) {
      return '💣';
    }

    if (cell.adjacentMines > 0) {
      return cell.adjacentMines;
    }

    return '';
  };

  const getNumberColor = (num: number) => {
    const colors = [
      '',
      'text-blue-600',
      'text-green-600',
      'text-red-600',
      'text-purple-600',
      'text-yellow-600',
      'text-pink-600',
      'text-gray-900',
      'text-gray-700',
    ];
    return colors[num] || '';
  };

  const cellClasses = cn(
    sizeClasses[size],
    'border border-gray-400 flex items-center justify-center font-bold cursor-pointer select-none transition-all duration-150',
    {
      'bg-gray-300 hover:bg-gray-200': cell.state === 'hidden',
      'bg-blue-500 hover:bg-blue-400': cell.state === 'flagged',
      'bg-gray-100': cell.state === 'revealed' && !cell.isMine,
      'bg-red-500': cell.state === 'revealed' && cell.isMine,
    },
    cell.state === 'revealed' && cell.adjacentMines > 0 && !cell.isMine && getNumberColor(cell.adjacentMines)
  );

  return (
    <div
      className={cellClasses}
      onClick={onClick}
      onContextMenu={onRightClick}
      role="button"
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          onClick();
        }
      }}
    >
      {getCellContent()}
    </div>
  );
}